"""Listen on a port, accept one connection, write the first record (ClientHello)
to /tmp/ch.bin (raw) and /tmp/ch.hex (hex, no pipes) for parser development."""
import socket, sys
port = int(sys.argv[1]) if len(sys.argv) > 1 else 14443
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", port)); s.listen(1)
c, _ = s.accept()
data = b""
while len(data) < 5 or len(data) < 5 + int.from_bytes(data[3:5],'big'):
    chunk = c.recv(4096)
    if not chunk: break
    data += chunk
open("/tmp/ch.bin","wb").write(data)
open("/tmp/ch.hex","w").write(data.hex())
c.close(); s.close()
