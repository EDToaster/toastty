from demo import *
ENV={"PS1":"$ ","PATH":"/tmp/rec/bin:"+os.environ["PATH"]}
SH=["bash","--norc","-i"]
def misc(t):
    t.type("clear; cat /tmp/rec/sel.txt\n"); time.sleep(1)
    t.click(21,0); t.type("showclip   # after a single click on KEEPME\n"); time.sleep(1)
    t.click(44,0,n=2); t.type("showclip   # double-click on cafe_x\n"); time.sleep(1)
    t.drag(72,1,4,2); t.type("showclip   # drag across the soft wrap (space at col 80)\n"); time.sleep(1)
    t.drag(7,3,9,3); t.type("showclip   # drag from right half of the first wide char\n"); time.sleep(1.5)
pair("select-misc","selection",SH,misc,ENV,lambda: setclip("KEEP (old clipboard)"),crop=360)
