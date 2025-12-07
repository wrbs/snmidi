mod data_packet;
mod message_queue;
mod protocol;

use std::net::{IpAddr, SocketAddr};

use clap::Parser;
use color_eyre::eyre::{OptionExt, Result, bail};
use midir::{MidiOutput, MidiOutputConnection, PortInfoError};
use midly::{MidiMessage, live::LiveEvent, stream::MidiStream};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{
        self, AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader,
        ReadHalf, WriteHalf,
    },
    net::{TcpListener, TcpStream, UdpSocket},
    sync::oneshot,
    task::JoinHandle,
    time::{Duration, Instant, Sleep, sleep_until},
};
use tracing::{Level, debug, enabled, error, info, instrument, warn};
use tracing_error::ErrorLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    data_packet::{Packet, QueuedMessage},
    message_queue::MessageQueue,
    protocol::{
        Ack, ConnectRequest, ConnectResponse, ErrorResponse, InitializeRequest,
        InitializeResponse, Port, PortId, ShutdownRequest,
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

async fn read_json<'a, 'b, T: Deserialize<'a> + 'a, S: AsyncBufRead + Unpin>(
    stream: &'b mut S,
    buf: &'a mut String,
) -> Result<T> {
    buf.clear();
    stream.read_line(buf).await?;

    let t = serde_json::from_str(buf)?;
    Ok(t)
}

async fn write_json<S: AsyncWrite + Unpin, T: Serialize>(
    stream: &mut S,
    value: &T,
) -> Result<()> {
    let mut data = serde_json::to_string(value)?;
    data.push('\n');
    stream.write_all(data.as_bytes()).await?;
    Ok(())
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

    fn send_queued_message_assume_valid(&mut self, raw_msg: &[u8]) -> Result<()> {
        if enabled!(Level::DEBUG) {
            self.buf.clear();
            let msg = LiveEvent::parse(raw_msg).expect("message already to be valid");
            debug!(?msg, "Sending message {:02X?}", raw_msg);
        }
        self.conn.send(raw_msg)?;
        Ok(())
    }

    fn send_message(&mut self, msg: LiveEvent) -> Result<()> {
        self.buf.clear();
        msg.write(&mut self.buf).unwrap();
        debug!(?msg, "Sending message {:02X?}", self.buf);
        self.conn.send(&self.buf)?;
        Ok(())
    }

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

async fn optional_sleep(sleep: Option<Sleep>) -> Option<()> {
    match sleep {
        None => None,
        Some(s) => {
            s.await;
            Some(())
        },
    }
}

async fn handle_client_result(
    mut read: BufReader<ReadHalf<TcpStream>>,
    write_: &mut WriteHalf<TcpStream>,
    bind: IpAddr,
) -> Result<()> {
    // TODO: this is all really messy, split into parts next time I touch

    let mut buffer = String::new();

    let InitializeRequest {
        client_name,
        version,
    } = read_json(&mut read, &mut buffer).await?;

    if version != 0 {
        bail!("Unsupported version {} (client={:?})", version, client_name);
    }

    info!(client_name, "initializing");
    let output = MidiOutput::new(&client_name)?;
    let ports = get_ports(&output);

    write_json(write_, &InitializeResponse { ports }).await?;

    let ConnectRequest { id } = read_json(&mut read, &mut buffer).await?;
    let port = output
        .find_port_by_id(id.0)
        .ok_or_eyre("port id not found")?;

    let mut connection = ConnectionWrapper::new(output.connect(&port, &client_name)?);
    let udp_sock = UdpSocket::bind((bind, 0)).await?;
    let udp_port = udp_sock.local_addr()?.port();
    info!(client_name, udp_port, "connected");
    write_json(write_, &ConnectResponse { udp_port }).await?;

    let mut udp_buffer = vec![0; 1500];
    let mut expected_seqnum = 0;
    let mut queue = MessageQueue::new();
    let mut t0: Option<Instant> = None;
    let mut instant_stream = MidiStream::new();

    // The shutdown reader goes in a oneshot as it's not cancel safe
    // if we need more messages we probably want a more general channel here
    // dealing with tcp reading

    let mut shutdown = {
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let result = read_json::<ShutdownRequest, _>(&mut read, &mut buffer).await;
            let _ = tx.send(result);
        });
        rx
    };

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
            biased; // shutdown > incoming packets > timer
            shutdown_result = &mut shutdown /* cancel safe: channel */ => {
                let ShutdownRequest { shutdown_stop_all } = shutdown_result??;
                connection.stop_all_on_exit = shutdown_stop_all;
                break;
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

                            for result in data_packet::read_queued_messages(data) {
                                let QueuedMessage {
                                    delta_time,
                                    messages,
                                } = result?;
                                queue.add(delta_time as u64, messages);
                            }
                        },
                    }

                    write_json(write_, &Ack { ack: seqnum }).await?;
                },
            }
        }
    }

    Ok(())
}

#[instrument(skip(stream))]
async fn handle_client(stream: TcpStream, client_addr: SocketAddr, bind: IpAddr) {
    let (read, mut write_) = io::split(stream);
    let read = BufReader::new(read);
    info!("Client connected");

    match handle_client_result(read, &mut write_, bind).await {
        Ok(()) => (),
        Err(error) => {
            error!(?error, "Client errored");
            let _ = write_json(&mut write_, &ErrorResponse {
                error: error.to_string(),
                details: format!("{:#?}", error),
                ansi: format!("{:?}", error),
            })
            .await;
        },
    }

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
