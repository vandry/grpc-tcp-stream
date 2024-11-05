# gRPC client and server for tunnelling a bidi byte stream

stdio ←grpc-tcp-stream→ gRPC tcpstream.Stream ←grpc-tcp-stream-receiver→ TCP/IP backend

I use this to connect to an SSH server inside a Kubernetes cluster
through a cluster Gateway that only passes gRPC. On the client side
I use `ProxyCommand` to talk to the `ssh` client over stdio.

# grpc-tcp-stream

Usage: `grpc-tcp-stream METHOD https://grpc-server/`

`METHOD` is used to select among multiple backends that the server
may support. The gRPC method name is rewritten to this value.

# grpc-tcp-stream-receiver

Usage:

```
grpc-tcp-stream-receiver COMPREHENSIVE-ARGS \
    --backend=Method1='[ip:addr:ess::]:port \
    --backend=Method2='[other:addr:ess::]:port
```

where `COMPREHENSIVE-ARGS` are arguments from
[comprehensive](https://docs.rs/comprehensive/latest/comprehensive/)
such as `--grpc-port` and `--diag-http-port`. `--backend` can be
repeated as many times as there are backends to dispatch to.
Currently the backend addresses have to be IPv6 socket addresses.
Use `[::ffff:a.b.c.d]:port` for IPv4.
