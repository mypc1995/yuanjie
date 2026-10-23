import socket, sys, time

def console(port, cmd, wait=1.0):
    s = socket.create_connection(("127.0.0.1", port), timeout=5)
    s.settimeout(wait)
    s.sendall((cmd + "\n").encode())
    buf = b""
    try:
        while True:
            chunk = s.recv(4096)
            if not chunk:
                break
            buf += chunk
    except socket.timeout:
        pass
    s.close()
    return buf.decode("utf-8", "replace")

print("=== hbbs console 21115: h ===")
print(console(21115, "h"))
print("=== hbbs console 21115: aur ===")
print(repr(console(21115, "aur")))
print("=== hbbs console 21115: rs ===")
print(repr(console(21115, "rs")))
print("=== hbbr console 21117: h ===")
print(console(21117, "h"))
print("=== hbbr console 21117: tb (带宽上限) ===")
print(repr(console(21117, "tb")))
