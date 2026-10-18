from demo import *
ENV={"PS1":"$ ","PATH":"/tmp/rec/bin:"+os.environ["PATH"]}
SH=["bash","--norc","-i"]
def less(t):
    t.type("seq -f 'row %g of less output' 1 200 | less\n"); time.sleep(1.5)
    t.drag(0,2,10,5); time.sleep(1.2); t.key("q"); time.sleep(0.5); t.type("showclip\n"); time.sleep(1.5)
pair("select-less","selection",SH,less,ENV,lambda: setclip("(old clipboard)"),crop=300)
def vimshift(t):
    t.type("vim -u NONE -N -c 'set mouse=a' /tmp/rec/fox.txt\n"); time.sleep(2)
    t.drag(5,2,30,4,mods=("shift",)); time.sleep(1.5)
    t.key("Escape"); t.type(":q!\n"); time.sleep(0.8); t.type("showclip\n"); time.sleep(1.5)
pair("select-shift-vim","selection",SH,vimshift,ENV,lambda: setclip("(old clipboard)"))
def herdr(t):
    time.sleep(3); t.key("Return"); time.sleep(1.5)
    t.type("clear; head -6 /tmp/rec/fox.txt\n"); time.sleep(1.5)
    t.drag(27,2,50,5,mods=("shift",)); time.sleep(1.5)
    t.type("showclip\n"); time.sleep(2)
pair("select-shift-herdr","selection",["/tmp/rec/herdr.sh"],herdr,ENV,lambda: setclip("(old clipboard)"))
def misc(t):
    t.type("clear; cat /tmp/rec/sel.txt\n"); time.sleep(1)
    t.click(22,1); t.type("showclip   # after a single click\n"); time.sleep(1)
    t.click(46,1,n=2); t.type("showclip   # double-click on cafe_x\n"); time.sleep(1)
    t.drag(70,2,9,3); t.type("showclip   # drag across the soft wrap\n"); time.sleep(1)
    t.drag(7,4,8,4,dx0=0.9); t.type("showclip   # drag from right half of a wide char\n"); time.sleep(1.5)
pair("select-misc","selection",SH,misc,ENV,lambda: setclip("KEEP (old clipboard)"),crop=360)
