#!/usr/bin/env python3
"""Minimal Minecraft 1.21.4 client used to validate mc-proxy against real Paper.

It performs the full login through the proxy (including the configuration
state) and prints the packets it sees.  A successful run ends with
"REAL PAPER PLAY STATE REACHED THROUGH MC-PROXY".

Usage:
    python3 scripts/paper-smoke-client.py [proxy-host] [proxy-port]

The protocol version (769 = 1.21.4) and the configuration-state packet ids are
version specific; adjust them when testing another backend version.
"""
import socket, struct, zlib, json, uuid, sys

PROTOCOL = 769
HOST = sys.argv[1] if len(sys.argv) > 1 else '127.0.0.1'
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 25599
VIEW_DISTANCE = 8

def vi(n):
    n &= 0xffffffff; out=b''
    while True:
        if n & ~0x7f == 0: return out+bytes([n])
        out += bytes([(n&0x7f)|0x80]); n >>= 7

def read_vi_bytes(buf, i):
    num=0; shift=0
    while True:
        b=buf[i]; i+=1; num |= (b&0x7f)<<shift; shift+=7
        if not b&0x80: return num, i

def s(x):
    b=x.encode(); return vi(len(b))+b

class Conn:
    def __init__(self, host, port):
        self.s = socket.create_connection((host, port), timeout=20)
        self.comp = None
    def send(self, pid, payload=b''):
        body = vi(pid)+payload
        if self.comp is not None:
            if len(body) >= self.comp:
                frame = vi(len(body)) + zlib.compress(body)
            else:
                frame = vi(0) + body
        else:
            frame = body
        self.s.sendall(vi(len(frame)) + frame)
    def recv_exact(self, n):
        out=b''
        while len(out) < n:
            chunk=self.s.recv(n-len(out))
            if not chunk: raise EOFError
            out += chunk
        return out
    def recv_varint(self):
        num=0; shift=0
        while True:
            b = self.recv_exact(1)[0]
            num |= (b & 0x7f) << shift
            if not b & 0x80:
                return num
            shift += 7
    def recv(self):
        ln = self.recv_varint()
        data = self.recv_exact(ln)
        if self.comp is not None:
            dlen, j = read_vi_bytes(data, 0)
            payload = data[j:] if dlen == 0 else zlib.decompress(data[j:])
        else:
            payload = data
        pid, j = read_vi_bytes(payload, 0)
        return pid, payload[j:]

name = 'PaperTest'
player_uuid = uuid.uuid4()
c = Conn(HOST, PORT)
c.send(0, vi(PROTOCOL)+s('localhost')+struct.pack('>H',25565)+vi(2))
c.send(0, s(name)+player_uuid.bytes)
print('login start sent')

state = 'login'
config_seen = 0
play_seen = 0
for _ in range(80):
    pid, data = c.recv()
    print(f'  <- [{state}] packet 0x{pid:02x} ({len(data)} bytes)')
    if state == 'login':
        if pid == 0x03:
            thr, _ = read_vi_bytes(data, 0)
            c.comp = thr
            print('     set compression', thr)
        elif pid == 0x02:
            u = uuid.UUID(bytes=data[:16])
            ln, i = read_vi_bytes(data, 16)
            nm = data[i:i+ln].decode()
            print(f'     login success uuid={u} name={nm}')
            c.send(0x03)  # login acknowledged
            state = 'config'
            # Client Information (configuration 0x00)
            info = s('en_us') + bytes([VIEW_DISTANCE]) + vi(0) + b'\x01' + bytes([0x7f]) + vi(1) + b'\x00' + b'\x01' + vi(0)
            c.send(0x00, info)
            print('     -> client information')
        elif pid == 0x00:
            ln, i = read_vi_bytes(data, 0)
            print('     DISCONNECT:', data[i:i+ln].decode(errors='replace')[:300]); sys.exit(1)
    elif state == 'config':
        config_seen += 1
        if pid == 0x0e:  # known packs
            c.send(0x07, vi(0))
            print('     -> known packs (0)')
        elif pid == 0x04:  # keep alive
            c.send(0x04, data[:8])
        elif pid == 0x05:  # ping
            c.send(0x05, data[:4])
        elif pid == 0x03:  # finish configuration
            c.send(0x03)
            state = 'play'
            print('     -> acknowledge finish configuration; entering PLAY')
        elif pid == 0x02:
            ln, i = read_vi_bytes(data, 0)
            print('     CONFIG DISCONNECT:', data[i:i+ln].decode(errors='replace')[:300]); sys.exit(1)
    else:
        play_seen += 1
        if pid == 0x1f or pid == 0x20:  # keep alive-ish
            pass
        if play_seen >= 3:
            break

print(f'config packets={config_seen} play packets={play_seen}')
if play_seen > 0:
    print('REAL PAPER PLAY STATE REACHED THROUGH MC-PROXY')
    sys.exit(0)
print('did not reach play state')
sys.exit(1)
