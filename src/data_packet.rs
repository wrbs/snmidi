use midly::stream::MidiStream;

use crate::message_queue::MessageQueue;

#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Instant { reset_queue: bool },
    Queue,
}

#[derive(Debug)]
pub struct Packet<'a> {
    pub seqnum: u32,
    pub kind: Kind,
    pub data: &'a [u8],
}

impl<'a> Packet<'a> {
    pub fn parse_header(data: &'a [u8]) -> Option<Self> {
        // HEADER- 8 bytes
        // 'SNM' + kind byte(i/q/r) + seqnum
        if data.len() < 8 {
            return None;
        }

        if &data[0..3] != b"SNM" {
            return None;
        }

        let kind = match data[3] {
            b'i' => Kind::Instant { reset_queue: false },
            b'q' => Kind::Queue,
            b'r' => Kind::Instant { reset_queue: true },
            _ => return None,
        };

        let seqnum = u32::from_be_bytes((&data[4..8]).try_into().unwrap());
        Some(Self {
            seqnum,
            kind,
            data: &data[8..],
        })
    }
}

pub struct QueueParser {
    stream: MidiStream,
    header_bytes_read: u8,
    delta: u16,
    remaining: u16,
}

impl QueueParser {
    pub fn new() -> Self {
        Self {
            stream: MidiStream::new(),
            header_bytes_read: 0,
            delta: 0,
            remaining: 0,
        }
    }

    pub fn reset(&mut self) {
        self.stream.flush(|_| {});
        self.header_bytes_read = 0;
    }

    pub fn feed(&mut self, mut data: &[u8], queue: &mut MessageQueue) {
        while !data.is_empty() {
            if self.header_bytes_read < 4 {
                let byte = *data.split_off_first().unwrap() as u16;

                match self.header_bytes_read {
                    0 => self.delta = byte << 8,
                    1 => {
                        self.delta |= byte;
                        queue.add_delta_time(self.delta as u64);
                    },
                    2 => self.remaining = byte << 8,
                    3 => self.remaining |= byte,
                    _ => unreachable!(),
                }

                self.header_bytes_read += 1;
            } else if self.remaining == 0 {
                self.stream.flush(|event| queue.enqueue(&event));
                self.header_bytes_read = 0;
            } else {
                let to_split = data.len().min(self.remaining as usize);
                self.remaining -= to_split as u16;
                let (cur, rest) = data.split_at(to_split);
                data = rest;
                self.stream.feed(cur, |event| queue.enqueue(&event));
            }
        }
    }
}
