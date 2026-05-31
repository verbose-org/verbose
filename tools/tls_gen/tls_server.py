"""A TLS 1.3 (PSK-DHE) server whose ENTIRE cryptography runs on Verbose-emitted
binaries (via vcrypto). The host does socket I/O + TLS message framing only.
Honest scope (docs/tls-io-statemachine-design.md §7): crypto primitives are pure
Verbose; framing + server randomness (os.urandom) are host glue.

Cipher suite TLS_AES_128_GCM_SHA256, X25519, external PSK (psk_dhe_ke).
Reachable by: openssl s_client -psk <hex32> -psk_identity test -tls1_3 \
  -ciphersuites TLS_AES_128_GCM_SHA256 -curves X25519 -connect 127.0.0.1:PORT
"""
import sys, os, socket, time, hmac, hashlib
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import vcrypto as V
from tlswire import ClientHello

PSK = bytes.fromhex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
BASEPOINT = bytes([9] + [0]*31)

def b16(x): return x.to_bytes(2,'big')
def b24(x): return x.to_bytes(3,'big')

def hs_msg(t, body): return bytes([t]) + b24(len(body)) + body
def record(ct, payload): return bytes([ct]) + b"\x03\x03" + b16(len(payload)) + payload

def hkdf_extract(salt, ikm):
    return V.run_bytes("hkdf_extract", [str(b) for b in salt]+[str(b) for b in ikm], 32)
def finished_key(secret):
    return V.run_bytes("finished_key", [str(b) for b in secret]+[str(b) for b in bytes(32)], 32)

def serve(conn):
    # 1. read ClientHello record
    data=b""
    while len(data) < 5 or len(data) < 5+int.from_bytes(data[3:5],'big'):
        ch_chunk=conn.recv(4096)
        if not ch_chunk: return
        data+=ch_chunk
    ch=ClientHello(data)
    ch_hs = ch.handshake
    if ch.x25519_pub is None or ch.psk_binder is None:
        print("no x25519 keyshare or no PSK"); return

    # 2. verify PSK binder (authenticate the peer) — all Verbose crypto
    early = V.run_bytes("psk_early_secret",[str(b) for b in PSK],32)
    binder_key = V.run_bytes("psk_ext_binder_key",[str(b) for b in early],32)
    bk_fin = finished_key(binder_key)
    th_trunc = V.sha256(ch.truncated_for_binder())
    binder = hkdf_extract(bk_fin, th_trunc)  # HMAC(bk_fin, th_trunc)
    if binder != ch.psk_binder:
        print("BINDER MISMATCH"); return
    print("binder OK")

    # 3. server ephemeral X25519 (host RNG = the one acknowledged host secret input)
    sk = bytearray(os.urandom(32)); sk[0]&=248; sk[31]&=127; sk[31]|=64
    server_pub = V.x25519(bytes(sk), BASEPOINT)
    ecdhe = V.x25519(bytes(sk), ch.x25519_pub)
    server_random = os.urandom(32)

    # 4. build ServerHello
    ks_ext = b16(0x0033)+b16(2+2+32)+b16(0x001d)+b16(32)+server_pub
    sv_ext = b16(0x002b)+b16(2)+b16(0x0304)
    psk_ext= b16(0x0029)+b16(2)+b16(0)   # selected_identity = 0
    exts = sv_ext+ks_ext+psk_ext
    sh_body = b"\x03\x03"+server_random+bytes([len(ch.legacy_session_id)])+ch.legacy_session_id \
              + b"\x13\x01" + b"\x00" + b16(len(exts))+exts
    sh_hs = hs_msg(0x02, sh_body)

    # 5. transcript = CH || SH (handshake-message bytes only)
    transcript = ch_hs + sh_hs

    # 6. key schedule (PSK-DHE)
    derived = V.run_bytes("derive_derived",[str(b) for b in early]+[str(b) for b in bytes(32)],32)
    handshake = hkdf_extract(derived, ecdhe)
    th_chsh = V.sha256(transcript)
    s_hs = V.run_bytes("derive_s_hs_traffic",[str(b) for b in handshake]+[str(b) for b in th_chsh],32)
    c_hs = V.run_bytes("derive_c_hs_traffic",[str(b) for b in handshake]+[str(b) for b in th_chsh],32)
    s_key=V.run_bytes("expand_key",[str(b) for b in s_hs],16); s_iv=V.run_bytes("expand_iv",[str(b) for b in s_hs],12)
    c_key=V.run_bytes("expand_key",[str(b) for b in c_hs],16); c_iv=V.run_bytes("expand_iv",[str(b) for b in c_hs],12)

    # 7. send ServerHello (plaintext) + dummy CCS
    conn.sendall(record(0x16, sh_hs))
    conn.sendall(record(0x14, b"\x01"))

    # 8. EncryptedExtensions (empty) + server Finished, encrypted under s_hs
    ee = hs_msg(0x08, b16(0))
    transcript += ee
    th_ee = V.sha256(transcript)
    sfk = finished_key(s_hs)
    verify_data = hkdf_extract(sfk, th_ee)   # HMAC(sfk, th_ee)
    fin = hs_msg(0x14, verify_data)
    transcript += fin
    flight = ee + fin
    conn.sendall(V.aead_encrypt(s_key, s_iv, 0, flight, 0x16))  # aead_encrypt already returns a full record
    print("sent SH + EE + Finished")

    # 9. read client's encrypted Finished, decrypt with c_hs
    rec = conn.recv(4096)
    if rec and rec[0]==0x14:  # client may send its own CCS first
        rec = conn.recv(4096)
    if rec:
        dec = V.aead_decrypt(c_key, c_iv, 0, rec[:5+int.from_bytes(rec[3:5],'big')])
        print("client finished decrypt:", "ok" if dec else "FAIL", dec[0] if dec else "")

    # 10. application data: master + app traffic secrets, respond "hello world"
    derived2 = V.run_bytes("derive_derived",[str(b) for b in handshake]+[str(b) for b in bytes(32)],32)
    master = hkdf_extract(derived2, bytes(32))
    th_full = V.sha256(transcript)
    s_ap = V.run_bytes("derive_s_ap_traffic",[str(b) for b in master]+[str(b) for b in th_full],32)
    s_ak=V.run_bytes("expand_key",[str(b) for b in s_ap],16); s_aiv=V.run_bytes("expand_iv",[str(b) for b in s_ap],12)
    http=b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nContent-Type: text/plain\r\n\r\nhello world"
    conn.sendall(V.aead_encrypt(s_ak, s_aiv, 0, http, 0x17))  # aead_encrypt already returns a full record
    print("sent application data (hello world)")
    time.sleep(2)  # let TCP deliver the record before close

def main():
    port=int(sys.argv[1]) if len(sys.argv)>1 else 14443
    V.ensure(V.ALL_RULES + [("psk_early_secret","psk_schedule.verbose"),
                            ("psk_ext_binder_key","psk_schedule.verbose"),
                            ("hkdf_extract","hkdf_extract.verbose")])
    s=socket.socket(socket.AF_INET,socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
    s.bind(("127.0.0.1",port)); s.listen(1)
    print(f"TLS-Verbose listening on {port}", flush=True)
    conn,_=s.accept()
    try: serve(conn)
    finally: conn.close(); s.close()

if __name__=="__main__":
    main()
