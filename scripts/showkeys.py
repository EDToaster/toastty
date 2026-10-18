#!/usr/bin/env python3
# showkeys.py <init-escapes> : raw mode, emit init, then print every input chunk readably. Ctrl-] quits.
import sys, os, tty, termios
init = sys.argv[1].encode().decode('unicode_escape').encode('latin-1') if len(sys.argv)>1 else b""
label = sys.argv[2] if len(sys.argv)>2 else ""
fd=0; old=termios.tcgetattr(fd); tty.setraw(fd)
def w(s): os.write(1, s.encode())
w("\x1b[2J\x1b[H"); os.write(1, init)
w(f"\x1b[1mbytes received by the app\x1b[0m  {label}\r\n\r\n")
def fmt(d):
    out=""
    for ch in d.decode('utf-8','backslashreplace'):
        o=ord(ch)
        if ch=="\x1b": out+="\x1b[33mESC\x1b[0m"
        elif o<32: out+=f"\x1b[31m^{chr(o+64)}\x1b[0m"
        elif o==127: out+="\x1b[31m^?\x1b[0m"
        elif 0x80<=o<0xa0: out+=f"\x1b[31m<{o:02x}>\x1b[0m"
        else: out+=ch
    return out
try:
    while True:
        d=os.read(fd,65536)
        if not d or b"\x1d" in d: break
        w("  "+fmt(d)+"\r\n")
finally:
    termios.tcsetattr(fd,termios.TCSADRAIN,old)
