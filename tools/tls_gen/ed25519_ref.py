"""Self-contained Ed25519 (RFC 8032) reference oracle — no external deps.
Used to validate each Verbose Ed25519 brick. Also exposes the Edwards point
ops / constants the Verbose generators mirror.
"""
import hashlib

p = 2**255 - 19
L = 2**252 + 27742317777372353535851937790883648493
d = (-121665 * pow(121666, p-2, p)) % p
I = pow(2, (p-1)//4, p)   # sqrt(-1)

def H(m): return hashlib.sha512(m).digest()
def inv(x): return pow(x, p-2, p)

# Extended twisted Edwards coords (X,Y,Z,T), a=-1, unified add.
def ed_add(P, Q):
    X1,Y1,Z1,T1 = P; X2,Y2,Z2,T2 = Q
    A = ((Y1-X1)*(Y2-X2)) % p
    B = ((Y1+X1)*(Y2+X2)) % p
    C = (T1*2*d*T2) % p
    Dd= (Z1*2*Z2) % p
    E = (B-A) % p; F = (Dd-C) % p; G = (Dd+C) % p; Hh = (B+A) % p
    return ((E*F)%p, (G*Hh)%p, (F*G)%p, (E*Hh)%p)

def scalarmult(P, e):
    Q = (0,1,1,0)  # neutral
    while e > 0:
        if e & 1: Q = ed_add(Q, P)
        P = ed_add(P, P)
        e >>= 1
    return Q

# base point B
By = (4 * inv(5)) % p
Bx = recover_x = None
def xrecover(y):
    xx = ((y*y-1) * inv(d*y*y+1)) % p
    x = pow(xx, (p+3)//8, p)
    if (x*x - xx) % p != 0: x = (x*I) % p
    if x & 1: x = p - x
    return x
Bx = xrecover(By)
B = (Bx, By, 1, (Bx*By)%p)

def encodepoint(P):
    x,y,z,t = P; zi = inv(z)
    x = (x*zi)%p; y = (y*zi)%p
    return bytes(((y >> (8*i)) & 0xff for i in range(31))) + bytes([ (y>>248)&0x7f | ((x&1)<<7) ])

def encodeint(e):
    return bytes((e >> (8*i)) & 0xff for i in range(32))

def secret_to_pub(sk):
    h = H(sk)
    a = int.from_bytes(h[:32],'little')
    a &= (1<<254) - 8; a |= (1<<254)
    A = scalarmult(B, a)
    return encodepoint(A)

def sign(sk, msg):
    h = H(sk)
    a = int.from_bytes(h[:32],'little'); a &= (1<<254)-8; a |= (1<<254)
    prefix = h[32:]
    A = encodepoint(scalarmult(B, a))
    r = int.from_bytes(H(prefix+msg),'little') % L
    R = encodepoint(scalarmult(B, r))
    k = int.from_bytes(H(R+A+msg),'little') % L
    S = (r + k*a) % L
    return R + encodeint(S)

if __name__ == "__main__":
    # RFC 8032 Test 1
    sk = bytes.fromhex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
    pub = bytes.fromhex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
    sig = bytes.fromhex("e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b")
    got_pub = secret_to_pub(sk)
    got_sig = sign(sk, b"")
    print("pub", "OK" if got_pub==pub else "FAIL", got_pub.hex()[:16])
    print("sig", "OK" if got_sig==sig else "FAIL", got_sig.hex()[:16])
    import sys
    sys.exit(0 if got_pub==pub and got_sig==sig else 1)
