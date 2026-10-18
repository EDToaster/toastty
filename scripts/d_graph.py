from demo import *
ENV={"PS1":"$ ","PATH":"/tmp/rec/bin:"+os.environ["PATH"]}
SH=["bash","--norc","-i"]
def g(t):
    t.type("clear; cat /tmp/rec/graph.txt\n"); time.sleep(1)
    t.drag(0,0,30,0); t.type("showclip   # combining marks, ZWJ family, VS16, Devanagari\n"); time.sleep(1.2)
    t.drag(0,1,22,1); t.type("showclip   # tab-separated line\n"); time.sleep(1.5)
pair("grapheme-copy","grapheme-copy",SH,g,ENV,crop=260)
def kitty(t):
    time.sleep(0.6); t.key("ctrl+shift+v"); time.sleep(0.8); t.key("ctrl+shift+c"); time.sleep(0.8); t.key("shift+Insert"); time.sleep(1.2)
pair("clip-kitty-keys","clipboard-osc52",["/tmp/rec/showkeys.py","\\x1b[?2004h\\x1b[>7u","(kitty >7u)"],kitty,ENV,lambda:(setclip("clipboard-text"),setclip("(old primary)","primary")),crop=220)
