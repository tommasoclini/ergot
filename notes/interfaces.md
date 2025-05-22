# Interfaces

Now that we have sockets, I should probably figure out interfaces, and generally how to figure out "on the wire" packets.

## A reminder on Addresses

Our Address type is very much inspired by AppleTalk's address format. It is made up of three parts:

* network id (16b)
* node id (8b)
* port/socket addr (8b)

### networks

"network ids" are generally defined as a "network segment". A network segment is any medium where multiple nodes can "hear" each other.

network IDs are expected to be automatically negotiated, though "how" isn't defined yet.

For example: an RS-485 bus would be a single "network", for example with up to 32 devices. A point to point ethernet link would be a single "network", with up to two devices.

The network IDs `0` and `65535` are reserved. All other IDs are considered valid.

The network ID `0` is defined as "the current network", typically used before a network ID is negotiated, or when sending messages within a single device. Messages with a destination network ID 0 should never be routed to other networks.

### nodes

Nodes are individual computers/entities on a network.

Node IDs are USUALLY expected to be automatically negotiated, using a technique similar to what appletalk does, basically each node will guess an ID, then shout out "is anyone using node ID x?", and see if anyone complains. If nobody complains after a couple attempts, then that node id will start being used.

Node IDs are designed to be amenable to re-use as an ID used on some local communication medium. For example: I2C addresses, RS-485/modbus addresses, SPI bus CS pin indexes, etc.

In cases where the local medium demands it (like i2c), node IDs may be statically defined, instead of automatically negotiated. These busses will not generally allow for dynamically adding new devices.

node ids `0` and `255` are reserved. All other IDs are considered valid.

The node ID `0` is defined as "the current node", typically used for messages within a single device. Messages with a destination node ID 0 should never be routed to other devices.

The node id `255` is defined as a "broadcast" message, to be processed by all nodes on a given network.

### (network, node) tuples

In general, every (network, node) tuple will be unique on an entire internet.

A single device may have multiple (network, node) tuples that "belong" to it, one per interface.

### ports

a "port id" or "socket id" identifies a single socket interface that can receive messages.

port ids are assigned by the device itself. port ids are the same across all interfaces of a device.

Port IDs `0` and `255` are reserved. All other IDs are considered valid.

The port id `0` is defined as "any port". This is explained more later.

There is currently no way for a single device to have more than 254 active ports. If you have a device that can or needs to do that, it's probably worth just using ipv4/ipv6.

todo: should we reserve any ports for special things? 1..=31 or something?

## On the wire

We definitely need some information in the header of packets.

* src address (32 bits)
* dst address (32 bits)
* sequence number (?? bits)

We might want some additional information in packets:

* a header and/or payload checksum, separate from a wire checksum (16-64 bits)
* some kind of version identifier for packets (4-8 bits)
* some kind of message kind/"protocol" (e.g. endpoint req/resp, topic message) identifier, (4-8 bits)
    * todo: use for things like sessionful connections?
* TTL counter (4-8 bits)
* QoS number (4-8 bits)

### Address compression?

todo: we could use a varint to encode the entire 32-bit (network, node, port) triplet. This would allow us to send addresses (0, 0, 1..=127) in one byte, and (0, 0..=63, 1..=254) in two bytes. The worst case (65534, 254, 254) is 5 bytes.

This means in many cases of point to point and local networks, we could support up to 63 nodes with only 2 byte addresses.

### `postcard-rpc` keys?

todo: we want to use postcard-rpc `Key`s in a load-bearing way. but they are 8 bytes, and that's a lot.

Maybe we only use that in cases where we want to use port id = 0, to auto-detect the correct port id? The requestor would then see the actual port id in the response.

## The interface interface

todo: what should the code interface of interfaces look like?

Interfaces need to:

* Do some kind of self-driven operation, like negotiating node ids or discovering network ids
* incoming packet path
    * In a pipelined manner:
        * Receive incoming packets, shove them in a queue (maybe in an interrupt)
        * Move packets from the queue into the netstack (probably in non-interrupt context)
    * In a non-pipelined manner:
        * recv frame, shove it into the netstack
* outgoing packet path
    * In a pipelined manner:
        * Have a handle (given to the netstack) that allows for shoving frames into an outgoing queue
        * Move packets from the queue to the wire (maybe in interrupt context)
    * In a non-pipelined manner:
        * I dunno how to do this with the blocking mutex

