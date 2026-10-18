from demo import *
import shutil
ENV={"PS1":"$ ","PATH":"/tmp/rec/bin:"+os.environ["PATH"]}
SH=["bash","--norc","-i"]
def pre():
    shutil.rmtree("/tmp/demo",ignore_errors=True); os.makedirs("/tmp/demo")
    setclip(b"echo hi\x1b[201~\x15touch /tmp/demo/PWNED\r")
def inject(t):
    t.type("ls /tmp/demo   # empty dir\n"); t.type("showclip\n"); time.sleep(1)
    t.type("# now paste with Ctrl+Shift+V (no Enter)"); t.key("ctrl+u"); time.sleep(0.5)
    t.key("ctrl+shift+v"); time.sleep(2.5)
    t.key("ctrl+c"); t.type("ls /tmp/demo\n"); time.sleep(1.5)
pair("paste-injection","paste-sanitize",SH,inject,ENV,pre)
def pre2(): setclip(b"line1\nline2\r\nline3\x03\x04\x1b[A\x7f")
def ctrls(t):
    time.sleep(0.5); t.key("ctrl+shift+v"); time.sleep(2)
pair("paste-controls","paste-sanitize",["/tmp/rec/showkeys.py","","(raw mode, no bracketed paste)"],ctrls,ENV,pre2)
