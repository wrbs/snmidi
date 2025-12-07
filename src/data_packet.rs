use color_eyre::eyre::{Result, eyre};

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

pub struct QueuedMessage<'a> {
    pub delta_time: u16,
    pub messages: &'a [u8],
}

pub fn read_queued_messages(
    mut data: &[u8],
) -> impl Iterator<Item = Result<QueuedMessage<'_>>> {
    std::iter::from_fn(move || {
        if data.len() == 0 {
            None
        } else {
            let cur = data;
            let result = if cur.len() <= 4 {
                Err(eyre!(
                    "expected 0 or >= 4 bytes while reading queued message: \
                     {data:02X?}"
                ))
            } else {
                let (delta_bytes, cur) = cur.split_at(2);
                let (length_bytes, cur) = cur.split_at(2);
                let delta_time = u16::from_be_bytes(delta_bytes.try_into().unwrap());
                let length = u16::from_be_bytes(length_bytes.try_into().unwrap());
                match cur.split_at_checked(length as usize) {
                    None => {
                        let actual_length = cur.len();
                        Err(eyre!(
                            "not all length bytes included: delta={delta_time}, \
                             length in packet={length}, \
                             actual_remaining={actual_length}"
                        ))
                    },
                    Some((messages, rest)) => {
                        data = rest;
                        Ok(QueuedMessage {
                            delta_time,
                            messages,
                        })
                    },
                }
            };
            if result.is_err() {
                data = &[];
            }
            Some(result)
        }
    })
}
