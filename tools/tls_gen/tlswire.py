"""TLS 1.3 wire parsing/serialization (host glue — no crypto here).

Parses a ClientHello and builds ServerHello/records. All cryptographic
transforms are done by vcrypto (the Verbose binaries); this module only does
length-prefixed byte framing per RFC 8446.
"""
import struct

def u16(b,o): return (b[o]<<8)|b[o+1]
def u24(b,o): return (b[o]<<16)|(b[o+1]<<8)|b[o+2]

class ClientHello:
    def __init__(self, record: bytes):
        assert record[0]==0x16, "not a handshake record"
        rlen=u16(record,3)
        self.handshake = record[5:5+rlen]
        hs=self.handshake
        assert hs[0]==0x01, "not ClientHello"
        hlen=u24(hs,1)
        body=hs[4:4+hlen]
        self.hs_body = body
        o=0
        self.legacy_version=u16(body,o); o+=2
        self.random=body[o:o+32]; o+=32
        sidlen=body[o]; o+=1
        self.legacy_session_id=body[o:o+sidlen]; o+=sidlen
        cslen=u16(body,o); o+=2
        self.cipher_suites=body[o:o+cslen]; o+=cslen
        complen=body[o]; o+=1
        self.compression=body[o:o+complen]; o+=complen
        extlen=u16(body,o); o+=2
        self.ext_start=4+o  # offset of extensions within `hs`
        exts=body[o:o+extlen]
        self.extensions={}        # type -> raw ext body
        self.ext_offsets={}       # type -> (start,end) within hs_body
        eo=0
        while eo+4<=len(exts):
            et=u16(exts,eo); el=u16(exts,eo+2)
            self.extensions[et]=exts[eo+4:eo+4+el]
            self.ext_offsets[et]=(o+eo, o+eo+4+el)
            eo+=4+el
        self._parse_key_share()
        self._parse_psk()

    def _parse_key_share(self):
        self.x25519_pub=None
        ks=self.extensions.get(0x0033)
        if not ks: return
        total=u16(ks,0); p=2
        while p+4<=2+total:
            grp=u16(ks,p); kl=u16(ks,p+2)
            if grp==0x001d:  # x25519
                self.x25519_pub=ks[p+4:p+4+kl]
            p+=4+kl

    def _parse_psk(self):
        self.psk_identity=None; self.psk_binder=None; self.binders_offset=None
        psk=self.extensions.get(0x0029)  # pre_shared_key (MUST be last ext)
        if not psk: return
        idlen=u16(psk,0); p=2
        # first PskIdentity
        ilen=u16(psk,p); self.psk_identity=psk[p+2:p+2+ilen]
        p_after_ids = 2+idlen
        # binders start at p_after_ids; each = 1-byte len + binder
        binders_len=u16(psk,p_after_ids)
        bstart=p_after_ids+2
        blen=psk[bstart]
        self.psk_binder=psk[bstart+1:bstart+1+blen]
        # absolute offset of the binders-list-length field within hs_body:
        ext_body_start = self.ext_offsets[0x0029][0]+4
        self.binders_offset = ext_body_start + p_after_ids  # points at binders_len (2B)

    def truncated_for_binder(self) -> bytes:
        """The ClientHello handshake message up to (not incl) the binders list,
        but WITH the length fields written as if binders were present. Per RFC
        8446 4.2.11.2 this is exactly hs[: <start of binders-list-length field>].
        i.e. the full message minus the binders-list content (the 2-byte
        binders_len and the binder bytes)."""
        # hs = 4-byte header + hs_body. binders_offset is within hs_body.
        cut = 4 + self.binders_offset  # within hs, points at binders_len(2B)
        return self.handshake[:cut]

if __name__ == "__main__":
    rec=open("/tmp/ch.bin","rb").read()
    ch=ClientHello(rec)
    print("legacy_session_id", ch.legacy_session_id.hex(), len(ch.legacy_session_id))
    print("cipher_suites", ch.cipher_suites.hex())
    print("x25519_pub", ch.x25519_pub.hex() if ch.x25519_pub else None,
          len(ch.x25519_pub) if ch.x25519_pub else 0)
    print("psk_identity", ch.psk_identity.hex() if ch.psk_identity else None)
    print("psk_binder", ch.psk_binder.hex() if ch.psk_binder else None,
          len(ch.psk_binder) if ch.psk_binder else 0)
    print("truncated_len", len(ch.truncated_for_binder()))
    # sanity: x25519 pub is 32 bytes, binder is 32 (SHA-256), session id 32
    ok = (ch.x25519_pub and len(ch.x25519_pub)==32 and ch.psk_binder and
          len(ch.psk_binder)==32 and 0x1301 in [ (ch.cipher_suites[i]<<8|ch.cipher_suites[i+1]) for i in range(0,len(ch.cipher_suites),2)])
    print("PARSE_OK" if ok else "PARSE_FAIL")
