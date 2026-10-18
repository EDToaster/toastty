from demo import *
ENV={"PS1":"$ ","PATH":"/tmp/rec/bin:"+os.environ["PATH"]}
def legacy(t):
    time.sleep(0.5); t.click(4,4); time.sleep(0.6); t.click(30,8); time.sleep(1)
pair("mouse-legacy","mouse-reporting",["/tmp/rec/showkeys.py","\\x1b[?1000h","(app enabled ?1000h only, no SGR 1006)"],legacy,ENV,crop=240)
def vim(t):
    time.sleep(1.5); t.click(20,9); time.sleep(1); t.click(8,3); time.sleep(1.5)
pair("mouse-legacy-vim","mouse-reporting",["env","TERM=screen","vim","-u","NONE","-N","-c","set mouse=a ttymouse=xterm2 ruler number","/tmp/rec/fox.txt"],vim,ENV)
def ctrl(t):
    time.sleep(0.5); t.click(20,6,mods=("ctrl",)); time.sleep(0.6); t.click(40,8,mods=("ctrl",)); time.sleep(1)
pair("mouse-ctrl-click","mouse-reporting",["/tmp/rec/showkeys.py","\\x1b[?1002h\\x1b[?1006h","(?1002h ?1006h; Ctrl+click on plain text)"],ctrl,ENV,crop=240)
