from demo import *
ENV={"PS1":"$ ","PATH":"/tmp/rec/bin:"+os.environ["PATH"]}
SH=["bash","--norc","-i"]
def pre(): setclip("clipboard-text"); setclip("(old primary)","primary")
def middle(t):
    t.type("echo select-me-then-middle-click\n"); time.sleep(0.8)
    t.click(2,1,n=2); time.sleep(0.4)
    t.type("echo pasted: "); t.click(30,6,button=2); time.sleep(0.8); t.key("Return"); time.sleep(0.6)
    t.type("showclip primary\n"); time.sleep(1.5)
pair("clip-middle-click","clipboard-osc52",SH,middle,ENV,pre,crop=240)
def osc(t):
    t.type("printf '\\e]52;p;%s\\a' $(printf to-primary | base64)   # OSC 52 'p'\n"); time.sleep(0.6)
    t.type("showclip primary; showclip\n"); time.sleep(1)
    t.type("printf '\\e]52;c;aGVsbG8\\a'   # unpadded base64 for 'hello'\n"); time.sleep(0.6)
    t.type("showclip\n"); time.sleep(1.5)
pair("clip-osc52","clipboard-osc52",SH,osc,ENV,pre,crop=280)
def kitty(t):
    time.sleep(0.6); t.key("ctrl+shift+v"); time.sleep(0.8); t.key("ctrl+shift+c"); time.sleep(0.8); t.key("shift+Insert"); time.sleep(1.2)
pair("clip-kitty-keys","clipboard-osc52",["/tmp/rec/showkeys.py","\\x1b[?2004h\\x1b[>7u","(kitty kbd >7u: Ctrl+Shift+V, Ctrl+Shift+C, Shift+Insert)"],kitty,ENV,pre,crop=220)
