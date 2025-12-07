use std::collections::VecDeque;

use midly::stream::MidiStream;

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
struct MessageQueueWithoutStream {
    data: VecDeque<u8>,
    entries: VecDeque<Entry>,
    queue_end_timestamp: u64,
    data_to_pop: usize,
}

impl MessageQueueWithoutStream {
    fn new() -> Self {
        Self {
            data: VecDeque::new(),
            entries: VecDeque::new(),
            queue_end_timestamp: 0,
            data_to_pop: 0,
        }
    }

    fn reset(&mut self) {
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

    fn advance_end(&mut self, delta: u64) {
        self.clear_used_data();
        self.queue_end_timestamp += delta;
    }

    fn enqueue(&mut self, data: &[u8]) {
        self.clear_used_data();
        if data.len() > 0 {
            self.data.extend(data);
            self.entries.push_back(Entry {
                timestamp: self.queue_end_timestamp,
                size: data.len(),
            });
        }
    }

    fn pop<'a>(&'a mut self, cur_timestamp: u64) -> PopResult<'a> {
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

pub struct MessageQueue {
    inner: MessageQueueWithoutStream,
    stream: MidiStream,
    buffer: Vec<u8>,
}

impl MessageQueue {
    pub fn new() -> Self {
        Self {
            inner: MessageQueueWithoutStream::new(),
            stream: MidiStream::new(),
            buffer: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.inner.reset();
    }

    pub fn add(&mut self, delta_time: u64, messages: &[u8]) {
        self.inner.advance_end(delta_time);
        self.stream.feed(messages, |msg| {
            self.buffer.clear();
            msg.write(&mut self.buffer).unwrap();
            self.inner.enqueue(&self.buffer);
        });
        self.stream.flush(|msg| {
            self.buffer.clear();
            msg.write(&mut self.buffer).unwrap();
            self.inner.enqueue(&self.buffer);
        });
    }

    pub fn pop(&mut self, cur_timestamp: u64) -> PopResult<'_> {
        self.inner.pop(cur_timestamp)
    }
}
