#!/usr/bin/env python3
"""Kitty graphics protocol test helper. Run INSIDE the terminal under test.

Subcommands:
  img OUT.png WxH [pattern|solid:RRGGBB|alpha|stripes]
        Generate a test image. `pattern` (default): 4 coloured quadrants
        (TL red, TR green, BL blue, BR yellow), white 1px border, black
        grid line every 10px, diagonal, and the size printed in the middle.
        Cropping vs squashing vs stretching is easy to tell apart.
  send KEYS [--png F | --raw24 F | --raw32 F | --b64 STR | --path F]
            [--chunk N] [--zlib] [--reply SECS] [--no-newline]
        Emit ESC _G KEYS ; payload ESC \\ .
        --png F    : payload = PNG bytes (adds f=100 if no f= in KEYS)
        --raw24 F  : decode image F, send RGB  (adds f=24,s=,v=)
        --raw32 F  : decode image F, send RGBA (adds f=32,s=,v=)
        --b64 STR  : payload is STR verbatim (already base64 or junk)
        --path F   : payload = base64(abs path of F) (use with t=f / t=t)
        --chunk N  : split base64 payload into N-byte chunks with m=1/m=0
                     (continuation chunks carry only m=, like kitten icat)
        --zlib     : zlib-compress the payload, adds o=z
        --reply S  : put tty in raw mode, read terminal replies for S secs,
                     print them (escaped) after the command
  raw STRING   Write STRING after Python unicode-escape decoding,
               e.g. kgp.py raw '\\x1b[2J\\x1b[H'
  query SECS   Send a CSI 14t / 16t / 18t and print the replies.
"""
import base64, os, select, sys, termios, time, tty, zlib

ESC = "\x1b"


def out(s):
    sys.stdout.write(s)
    sys.stdout.flush()


def make_img(path, size, kind="pattern"):
    from PIL import Image, ImageDraw
    w, h = (int(x) for x in size.lower().split("x"))
    if kind.startswith("solid:"):
        c = kind.split(":", 1)[1]
        rgb = tuple(int(c[i:i + 2], 16) for i in (0, 2, 4))
        Image.new("RGBA", (w, h), rgb + (255,)).save(path)
        return
    if kind == "alpha":
        im = Image.new("RGBA", (w, h))
        px = im.load()
        for y in range(h):
            for x in range(w):
                px[x, y] = (255, 0, 255, int(255 * x / max(1, w - 1)))
        im.save(path)
        return
    im = Image.new("RGBA", (w, h), (0, 0, 0, 255))
    d = ImageDraw.Draw(im)
    if kind == "stripes":
        for y in range(h):
            col = [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255)][(y // 10) % 4]
            d.line([(0, y), (w, y)], fill=col + (255,))
        d.rectangle([0, 0, w - 1, h - 1], outline=(255, 255, 0, 255))
        im.save(path)
        return
    hw, hh = w // 2, h // 2
    d.rectangle([0, 0, hw, hh], fill=(200, 30, 30, 255))
    d.rectangle([hw, 0, w, hh], fill=(30, 180, 30, 255))
    d.rectangle([0, hh, hw, h], fill=(30, 60, 200, 255))
    d.rectangle([hw, hh, w, h], fill=(220, 200, 20, 255))
    for x in range(0, w, 10):
        d.line([(x, 0), (x, h)], fill=(0, 0, 0, 255))
    for y in range(0, h, 10):
        d.line([(0, y), (w, y)], fill=(0, 0, 0, 255))
    d.line([(0, 0), (w - 1, h - 1)], fill=(255, 255, 255, 255), width=2)
    d.rectangle([0, 0, w - 1, h - 1], outline=(255, 255, 255, 255))
    d.text((hw - 20, hh - 6), f"{w}x{h}", fill=(255, 255, 255, 255))
    im.save(path)


class RawTty:
    def __enter__(self):
        self.fd = sys.stdin.fileno()
        self.old = termios.tcgetattr(self.fd)
        tty.setraw(self.fd)
        return self

    def read_for(self, secs):
        buf = b""
        end = time.time() + secs
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], max(0, end - time.time()))
            if r:
                buf += os.read(self.fd, 65536)
        return buf

    def __exit__(self, *a):
        termios.tcsetattr(self.fd, termios.TCSADRAIN, self.old)


def apc(keys, payload=""):
    return f"{ESC}_G{keys};{payload}{ESC}\\" if payload or keys else ""


def send(args):
    keys = args[0]
    rest = args[1:]
    payload = b""
    chunk = 0
    use_zlib = False
    reply = 0.0
    newline = True
    i = 0
    extra = []
    b64_verbatim = None
    while i < len(rest):
        a = rest[i]
        if a == "--png":
            payload = open(rest[i + 1], "rb").read()
            if "f=" not in keys:
                extra.append("f=100")
            i += 2
        elif a in ("--raw24", "--raw32"):
            from PIL import Image
            im = Image.open(rest[i + 1])
            mode = "RGB" if a == "--raw24" else "RGBA"
            im = im.convert(mode)
            payload = im.tobytes()
            extra.append(f"f={24 if mode == 'RGB' else 32},s={im.width},v={im.height}")
            i += 2
        elif a == "--b64":
            b64_verbatim = rest[i + 1]
            i += 2
        elif a == "--path":
            payload = os.path.abspath(rest[i + 1]).encode()
            i += 2
        elif a == "--chunk":
            chunk = int(rest[i + 1])
            i += 2
        elif a == "--zlib":
            use_zlib = True
            extra.append("o=z")
            i += 1
        elif a == "--reply":
            reply = float(rest[i + 1])
            i += 2
        elif a == "--no-newline":
            newline = False
            i += 1
        else:
            sys.exit(f"unknown arg {a}")
    if use_zlib:
        payload = zlib.compress(payload)
    full_keys = ",".join([k for k in [keys] + extra if k])
    data = b64_verbatim if b64_verbatim is not None else base64.standard_b64encode(payload).decode()
    if chunk and len(data) > chunk:
        parts = [data[j:j + chunk] for j in range(0, len(data), chunk)]
        seq = ""
        for n, part in enumerate(parts):
            more = 1 if n < len(parts) - 1 else 0
            k = f"{full_keys},m={more}" if n == 0 else f"m={more}"
            seq += f"{ESC}_G{k};{part}{ESC}\\"
    else:
        seq = f"{ESC}_G{full_keys};{data}{ESC}\\"
    if reply:
        with RawTty() as t:
            out(seq)
            got = t.read_for(reply)
        out("\r\n" if newline else "")
        out("REPLY: " + repr(got) + "\r\n")
    else:
        out(seq)


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return
    cmd = sys.argv[1]
    if cmd == "img":
        make_img(sys.argv[2], sys.argv[3], sys.argv[4] if len(sys.argv) > 4 else "pattern")
    elif cmd == "send":
        send(sys.argv[2:])
    elif cmd == "raw":
        out(sys.argv[2].encode().decode("unicode_escape"))
    elif cmd == "query":
        secs = float(sys.argv[2]) if len(sys.argv) > 2 else 0.5
        with RawTty() as t:
            out(f"{ESC}[14t{ESC}[16t{ESC}[18t")
            got = t.read_for(secs)
        out("REPLY: " + repr(got) + "\r\n")
    else:
        print(__doc__)


if __name__ == "__main__":
    main()
