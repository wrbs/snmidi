use std::collections::VecDeque;

use midly::live::LiveEvent;
use tracing::debug;

#[derive(Debug)]
struct Entry {
    timestamp: u64,
    size: usize,
}

#[derive(Debug)]
pub enum PopResult<'a> {
    Message(&'a [u8]),
    Empty,
    NextTimestamp(u64),
}

#[derive(Debug)]
pub struct MessageQueue {
    data: VecDeque<u8>,
    entries: VecDeque<Entry>,
    queue_end_timestamp: u64,
    data_to_pop: usize,
    buffer: Vec<u8>,
}

impl MessageQueue {
    pub fn new() -> Self {
        Self {
            data: VecDeque::new(),
            entries: VecDeque::new(),
            queue_end_timestamp: 0,
            data_to_pop: 0,
            buffer: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.data.clear();
        self.entries.clear();
        self.queue_end_timestamp = 0;
        self.data_to_pop = 0;
    }

    fn clear_used_data(&mut self) {
        for _ in 0..self.data_to_pop {
            self.data.pop_front();
        }

        self.data_to_pop = 0;
    }

    pub fn add_delta_time(&mut self, delta: u64) {
        debug!(?delta, "add delta time");
        self.clear_used_data();
        self.queue_end_timestamp += delta;
    }

    pub fn enqueue(&mut self, message: &LiveEvent) {
        self.clear_used_data();
        self.buffer.clear();
        message.write(&mut self.buffer).unwrap();
        self.data.extend(self.buffer.as_slice());
        self.entries.push_back(Entry {
            timestamp: self.queue_end_timestamp,
            size: self.buffer.len(),
        });
    }

    pub fn pop<'a>(&'a mut self, cur_timestamp: u64) -> PopResult<'a> {
        self.clear_used_data();
        match self.entries.front() {
            None => PopResult::Empty,
            Some(entry) => {
                if entry.timestamp > cur_timestamp {
                    PopResult::NextTimestamp(entry.timestamp)
                } else {
                    let entry = self.entries.pop_front().unwrap();
                    let size = entry.size;
                    self.data_to_pop += size;
                    let hd_len = {
                        let (hd, _) = self.data.as_slices();
                        hd.len()
                    };
                    if hd_len < size {
                        let all = self.data.make_contiguous();
                        PopResult::Message(&all[..size])
                    } else {
                        let (hd, _) = self.data.as_slices();
                        PopResult::Message(&hd[..size])
                    }
                }
            },
        }
    }
}
