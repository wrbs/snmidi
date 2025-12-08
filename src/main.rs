mod data_packet;
mod json_lines;
mod message_queue;
mod protocol;

use std::net::{IpAddr, SocketAddr};

use clap::Parser;
use color_eyre::eyre::{OptionExt, Result, bail};
use midir::{MidiOutput, MidiOutputConnection, PortInfoError};
use midly::{MidiMessage, live::LiveEvent, stream::MidiStream};
use tokio::{
    net::{TcpListener, TcpStream, UdpSocket},
    task::JoinHandle,
    time::{Duration, Instant, Sleep, sleep_until},
};
use tracing::{Level, debug, enabled, error, info, instrument, warn};
use tracing_error::ErrorLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    data_packet::{Packet, QueueParser},
    json_lines::{JsonLines, ReadJson},
    message_queue::MessageQueue,
    protocol::{
        Ack, ConnectRequest, ConnectResponse, ErrorResponse, EstablishedCommand,
        InitializeRequest, InitializeResponse, Port, PortId,
    },
};

#[derive(clap::Parser)]
struct Args {
    /// TCP port to bind to
    #[clap(long, default_value = "4836")]
    port: u16,

    /// Address to bind to
    #[clap(long, default_value = "0.0.0.0")]
    bind: IpAddr,

    /// Show debug logs
    #[clap(long)]
    debug: bool,
}

fn get_ports(output: &MidiOutput) -> Vec<Port> {
    output
        .ports()
        .into_iter()
        .filter_map(|port| {
            let id = PortId(port.id());
            let name = match output.port_name(&port) {
                Ok(name) => Some(Some(name)),
                Err(err) => match err {
                    PortInfoError::PortNumberOutOfRange => None,
                    PortInfoError::InvalidPort => None,
                    PortInfoError::CannotRetrievePortName => Some(None),
                },
            }?;

            Some(Port { id, name })
        })
        .collect()
}

struct ConnectionWrapper {
    buf: Vec<u8>,
    conn: MidiOutputConnection,
    stop_all_on_exit: bool,
}

impl ConnectionWrapper {
    fn new(conn: MidiOutputConnection) -> Self {
        Self {
            buf: Vec::with_capacity(3),
            conn,
            stop_all_on_exit: true,
        }
    }

    #[instrument(skip(self))]
    fn send_queued_message_assume_valid(&mut self, raw_msg: &[u8]) -> Result<()> {
        if enabled!(Level::DEBUG) {
            self.buf.clear();
            let msg = LiveEvent::parse(raw_msg).expect("message already to be valid");
            debug!(?msg, "Sending queued message");
        }
        self.conn.send(raw_msg)?;
        Ok(())
    }

    #[instrument(skip(self))]
    fn send_message(&mut self, msg: LiveEvent) -> Result<()> {
        self.buf.clear();
        msg.write(&mut self.buf).unwrap();
        debug!("Sending message {:02X?}", self.buf);
        self.conn.send(&self.buf)?;
        Ok(())
    }

    #[instrument(skip(self))]
    fn panic(&mut self) -> Result<()> {
        for ch in 0..16 {
            self.send_message(LiveEvent::Midi {
                channel: ch.into(),
                message: MidiMessage::Controller {
                    controller: 123.into(),
                    value: 0.into(),
                },
            })?;
        }

        Ok(())
    }
}

impl Drop for ConnectionWrapper {
    fn drop(&mut self) {
        if self.stop_all_on_exit {
            if let Err(error) = self.panic() {
                error!(?error, "error while cleaning up in drop");
            }
        }
    }
}

#[instrument]
async fn optional_sleep(sleep: Option<Sleep>) -> Option<()> {
    match sleep {
        None => None,
        Some(s) => {
            s.await;
            Some(())
        },
    }
}

#[instrument(skip_all)]
async fn handle_client_result(
    json: &mut JsonLines<TcpStream>,
    bind: IpAddr,
) -> Result<()> {
    // TODO: this is all really messy, split into parts next time I touch

    let Some(InitializeRequest {
        client_name,
        version,
    }) = json.json_read_opt().await?
    else {
        return Ok(());
    };

    if version != 0 {
        bail!("Unsupported version {} (client={:?})", version, client_name);
    }

    info!(client_name, "initializing");
    let output = MidiOutput::new(&client_name)?;
    let ports = get_ports(&output);

    json.write(&InitializeResponse { ports }).await?;

    let Some(ConnectRequest { id }) = json.json_read_opt().await? else {
        return Ok(());
    };
    let port = output
        .find_port_by_id(id.0)
        .ok_or_eyre("port id not found")?;

    let mut connection = ConnectionWrapper::new(output.connect(&port, &client_name)?);
    let udp_sock = UdpSocket::bind((bind, 0)).await?;
    let udp_port = udp_sock.local_addr()?.port();
    info!(client_name, udp_port, "connected");
    json.write(&ConnectResponse { udp_port }).await?;

    let mut udp_buffer = vec![0; 1500];
    let mut expected_seqnum = 0;
    let mut queue = MessageQueue::new();
    let mut t0: Option<Instant> = None;
    let mut instant_stream = MidiStream::new();
    let mut queue_parser = QueueParser::new();

    // The shutdown reader goes in a oneshot as it's not cancel safe
    // if we need more messages we probably want a more general channel here
    // dealing with tcp reading

    loop {
        let sleep = if let Some(start_time) = t0 {
            let elapsed = start_time.elapsed();
            let cur_timestamp = elapsed.as_millis() as u64;
            loop {
                match queue.pop(cur_timestamp) {
                    message_queue::PopResult::Message(raw_msg) => {
                        connection.send_queued_message_assume_valid(raw_msg)?;
                    },
                    message_queue::PopResult::Empty => {
                        break None;
                    },
                    message_queue::PopResult::NextTimestamp(next_ts) => {
                        let sleep =
                            sleep_until(start_time + Duration::from_millis(next_ts));
                        break Some(sleep);
                    },
                }
            }
        } else {
            None
        };

        let read_bytes = tokio::select! {
            biased; // command > incoming packets > timer
            command_result = json.json_read_opt() /* cancel safe: channel */ => {
                match command_result? {
                    Some(EstablishedCommand::ShutdownWithoutStop) => {
                        connection.stop_all_on_exit = false;
                        return Ok(());
                    }
                    None => return Ok(()),
                }
            }
            read_result = udp_sock.recv(&mut udp_buffer[..]) /* cancel safe: documented */ => {
                let read_bytes = read_result?;
                Some(read_bytes)
            }
            Some(()) = optional_sleep(sleep) => None
        };

        if let Some(len) = read_bytes {
            let contents = &udp_buffer[..len];
            match Packet::parse_header(contents) {
                None => {
                    let start = &contents[..8];
                    warn!(len, "Got invalid packet, skipping. start = {start:02X?}");
                },
                Some(Packet { seqnum, kind, data }) => {
                    debug!(seqnum, ?kind, len = data.len(), "Received UDP packet");
                    if seqnum == expected_seqnum {
                        expected_seqnum += 1;
                    } else if seqnum == 0xDEADBEEF {
                        warn!("skipping seqnum mismatch because 0xDEADBEEF");
                    } else {
                        bail!(
                            "Unexpected seqnum {}; expected {}",
                            seqnum,
                            expected_seqnum
                        )
                    }

                    match kind {
                        data_packet::Kind::Instant { reset_queue } => {
                            if reset_queue {
                                queue.reset();
                                queue_parser.reset();
                                t0 = None;
                                instant_stream.flush(|_| {});
                            }

                            let mut error = None;
                            instant_stream.feed(data, |msg| {
                                if error.is_none() {
                                    if let Err(err) = connection.send_message(msg) {
                                        error = Some(err);
                                    }
                                }
                            });
                        },
                        data_packet::Kind::Queue => {
                            if t0.is_none() {
                                t0 = Some(Instant::now());
                            }

                            queue_parser.feed(data, &mut queue);
                        },
                    }

                    json.write(&Ack { ack: seqnum }).await?;
                },
            }
        }
    }
}

#[instrument(skip(stream))]
async fn handle_client(stream: TcpStream, client_addr: SocketAddr, bind: IpAddr) {
    let mut json = JsonLines::new(stream).await;
    info!("Client connected");

    match handle_client_result(&mut json, bind).await {
        Ok(()) => (),
        Err(error) => {
            error!(?error, "error while handling client");
            let _ = json.write(&ErrorResponse::of_report(&error)).await;
        },
    }

    let _ = json.join();

    info!("Finished handling client");
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    color_eyre::install()?;
    tracing_subscriber::fmt::fmt()
        .with_max_level(if args.debug {
            Level::DEBUG
        } else {
            Level::INFO
        })
        .finish()
        .with(ErrorLayer::default())
        .init();

    debug!("Debug logs enabled"); // only shown if true

    #[cfg(windows)]
    unsafe {
        winapi::um::timeapi::timeBeginPeriod(1);
    }

    let server: JoinHandle<Result<()>> = tokio::spawn(async move {
        let listener = TcpListener::bind((args.bind, args.port)).await?;
        let listen_addr = listener.local_addr()?;

        info!(addr = ?listen_addr, "Listening");
        loop {
            let (stream, client_addr) = listener.accept().await?;

            tokio::spawn(
                async move { handle_client(stream, client_addr, args.bind).await },
            );
        }
    });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Shutting down");
        },
        result = server => result??,
    }

    #[cfg(windows)]
    unsafe {
        winapi::um::timeapi::timeEndPeriod(1);
    }

    Ok(())
}
