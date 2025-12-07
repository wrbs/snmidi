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

## Usage

    cargo install --git https://github.com/wrbs/snmidi.git
    snmidi [--debug] [--bind 0.0.0.0] [--port 4836]

Debug shows logs of every midi message sent.

## Protocol

You connect over tcp, choose the port to connect to and then it tells you where
to send midi over udp.

You can have multiple connections at once.

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

(Well actually you get back `ansi` and `details` fields too, but that feels like
more of a rust implementation detail/not part of the 'protocol': feel free to
just ignore them and make them optional in any client libraries if you do)

### Shutdown

To gracefully shutdown, send one of the final shutdown messages. The server will
close the tcp session/udp socket/midi connection.

    > { "shutdown_stop_all": false }
    > { "shutdown_stop_all": true }

`stop_all` will send a midi note off to everything if true (like happens on
errors).

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

Queue messages consist of events with a 4-byte header specifying delta time in
ms (first 2 bytes) and length of the messages (next 2). Multiple messages can be
included with the same delta time.

Each packet can have multiple messages. Delta time runs on between packets.

The first delta time sent (or first sent after a reset packet) specifies the
offset when things will start playing -- set it some time in the future to
 have a chance to buffer things first.

Length can be zero if you have some need to send more than 65.535s of silence.

Example: two notes lasting 100ms one after the other

    printf 'SNMq\xDE\xAD\xBE\xEF\x00\x00\x00\x03\x90\x3C\x7F\x00\x64\x00\x05\x90\x3C\x00\x3E\x7F\x00\x64\x00\x03\x80\x3E\x00' | socat - UDP4-DATAGRAM:$host:$port

Split out

    'SNMq'     # magic number, type = q
    DEADBEEF # seqnum, this one is always considered 'valid'
    0000     # delta time = 0ms (start executing sequence immedaitely)
    0003     # len=3
    903C7F   # note on
    0064     # delta time = 100ms
    0005     # len=5
    903C00   # note on (vel=0 = off)
    3E7F     # next note on (using running status)
    0064     # another 100ms
    0003     # len=3
    803E00   # note off (this time using actual code)

Run with `--debug` to get a debug log of every message