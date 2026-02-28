# I/O Surface

High-level APIs:

- `spargio::fs::{OpenOptions, File}` + path helpers
- `spargio::net::{TcpListener, TcpStream, UdpSocket, Unix*}`
- `spargio::io::{AsyncRead, AsyncWrite, split, copy_to_vec, BufReader, BufWriter}`.

Low-level APIs:

- `RuntimeHandle::uring_native_unbound()` for direct operation submission.

For strict non-DNS data plane paths, use explicit `SocketAddr` connect/bind
entrypoints.
