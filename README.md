# snmidi - 'small network midi'

A server for a custom protocol for sending midi over the network. Born out of
frustration with the complexity of RTP-MIDI, driver bugs in ipmidi (although the
multicast idea is cool) and a desire to be able to queue messages to play back
to get around wifi jitter.

Exposes the server's own midi devices in a generic/cross-platform way using the
`midir` rust crate.

Uses TCP for session setup/connection status tracking/error notifications/acks
and UDP for sending midi packets.

Uses seqnums for detecting drops, but currently it'll just error out everything
if it detects it. You can check for drops using the acks on the client too.

Most novel thing is a way to queue messages to work around patchy wifi
connections for less strictly realtime things like sequencers.

Timing accurate to about ~1ms, network and OS scheduler dependent (it does use
`timeBeginPeriod` of `1ms` on Windows).

Only sending midi from client->server is supported for now. May extend in the
future.

## Usage

    cargo install --git https://github.com/wrbs/snmidi
    snmidi [--debug] [--bind 0.0.0.0] [--port 4836]

Debug shows logs of every midi message sent.

## Protocol

Clients connect over tcp (default 4836), choose the midi port to connect to from
the ones the host driver presents and then send udp packets with either

- raw midi messages (with a very small header)
- 

You can have multiple connections at once: each connection will have a different
udp port the server tells you to use.

### initialization

TCP control is json lines. Connect and send the start message

    > {"client_name":"test", "version": 0}

Server responds with your devices

    < {"ports":[{"id":"\u0000","name":"Microsoft GS Wavetable Synth"}]}

Specify which one to connect to by id and start the session

    > {"id":"\u0000"}

Then you can connect on the udp port it tells you

    < {"udp_port":60099}

### Errors

Every error is handled the same way (either in response to a command or
asynchronously). The server sends a final notification with
the error message and shuts everything down.

    < {"error":"expected value at line 1 column 1"}

If established, the connection is closed after sending a 'panic' note off CC
(`controller=123`) to every channel.

Feel free to reconnect.

(Well actually you get back `ansi` and `context` fields too, but that feels like
more of a rust implementation detail/not part of the 'protocol': feel free to
just ignore them and make them optional in any client libraries if you do)

### Shutdown

At any point you can close the stream to shutdown. By default if the connection
is established it will send a 'all notes off' midi message to every channel (CC
123).

To shutdown without stopping send `{"command": "shutdown_without_stop"}` as the
message. That's the only command currently; if you want the stop just close the
stream.

### udp header

3 types: instant (`i`), queue (`q`), reset (`r`)

Header is

    SNMxyyyy

where `x` = type code above, `yyyy` = 32 bit seqnum

Seqnum must start from 0 and increment each packet. Use `0xDEADBEEF` when
testing to disable the seqnum validation (example below)

### acks

The server sends acks over tcp after every message. Make sure to actually read
them or eventually the server send buffer will fill up and it'll block.

    < {"ack":0}
    < {"ack":1}
    < {"ack":2}

### Instant/Reset

Format is simple: after the header just raw midi data (wire format, not smf).
You can optionally use running status, messages get normalized by re-parsing with `midly`

You can split large sysex messages into multiple packets, the `midly` stream
parse state is retained in between packets.

Examples (note on/note off):

    printf 'SNMi\xDE\xAD\xBE\xEF\x90\x3c\x7F' | socat - UDP4-DATAGRAM:$host:$port

Reset is the same as instant but also clears the future message queue and resets
its start time. You can use the payload to also stop all notes/reset other CC
values too.

    printf 'SNMr\xDE\xAD\xBE\xEF' | socat - UDP4-DATAGRAM:$host:$port

Note reset does not reset the sequence number, only the queued messages.

### Queue

You can queue messages to run in the future with the server handling the timing.
This is useful if you already know what you want to play in the near future
(sequencer/playing back a midi file) and want to smooth over potential wifi
latency or jitter.  The minimum granularity is 1ms which nicely aligns with:

- the minimum OS's reasonably provide for context switches
- human perceptual limits
- usual LAN ethernet latency
- the time taken to send a single 3-byte 'note on' midi message over an actual
  midi cable (at the standard 31.25 kbps it's 960 µs)

Although this is the minimum timing granularity, relative ordering is preserved.

The way this works is you can send 'queue' packets which are annotated with
delta times (like midi files) which are always denoted in ms (unlike midi files,
no quarter notes here).  The server will play these back in addition to any
'instant' packets it receives according to these delta times.

Each packet can have multiple messages. Delta time runs on between packets, and messages can have partial entries/you can end the packets mid way through specifying anything: the parser state is retained in between packets.

Reset packets do reset the parser state and queue though.

Queue entries consist of events with a 4-byte header specifying delta time in
ms (first 2 bytes) and length of the messages (next 2). Multiple messages can be
included within the same record, the server splits them up into individual
messages when sending to the driver.

The time the first queue packet is received (or first sent after a reset)
is defined as `t0`. Every timestamp is considered relative to this point. 
This means the very first delta time specifies the delay before things will
start playing -- set it some time in the future to have a chance to buffer
things first.

Specifically, at any point where the server is processing things it will look
for any queued messages whose time is in the past (relative to current time -
`t0`) and execute them. Then it will sleep until the next queued message's time
or next UDP packet it receives, whichever is triggered first.

If the server receives a queue request for an event that is in the past relative
to the current time it will immediately execute.

In addition to the timer's schedule (every ms at most if needed), the queue is
checked after handling an instant packet. The messages in the instant packet
will execute first.

You can use running status within messages with the same header but not across
different ones.  Whatever you do it gets normalized before it gets sent to the
driver.

Length can be zero (so you can just submit a delta time if you want to queue up
more than 65.535s of silence for some reason).

Example: two notes lasting 100ms one after the other

    printf 'SNMq\xDE\xAD\xBE\xEF\x00\x00\x00\x03\x90\x3C\x7F\x00\x64\x00\x05\x90\x3C\x00\x3E\x7F\x00\x64\x00\x03\x80\x3E\x00' | socat - UDP4-DATAGRAM:$host:$port

Split out

    'SNMq'   # magic number, type = q
    DEADBEEF # seqnum, this one is always considered 'valid'

    0000     # delta time = 0ms (start executing sequence immediately)
    0003     # len=3
    903C7F   # note on

    0064     # delta time = 100ms
    0005     # len=5
    903C00   # note off (note on with vel=0)
    3E7F     # next note on (using running status of `90`)

    0064     # delta time = 100ms (200ms relative to start)
    0003     # len=3
    803E00   # note off (actual note off message this time)

Run with `--debug` to get a debug log of every midi message sent.

## Future ideas

- gap filler/retransmission for dropped packets
- receiving midi