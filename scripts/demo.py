# Recording framework. Usage in demo scripts:  from demo import *; record(name, binary, label, cmd, actions_fn)
import subprocess, time, os, signal, sys
sys.path.insert(0,"/tmp/aud")
os.environ["DISPLAY"]=":99"; os.environ["LANG"]="C.UTF-8"; os.environ["LC_ALL"]="C.UTF-8"
CW,CH=10,19
def sh(*a): return subprocess.run(a,capture_output=True,text=True).stdout
def ensure_x():
    if subprocess.run(["pgrep","-x","Xvfb"],capture_output=True).returncode!=0:
        subprocess.Popen(["Xvfb",":99","-screen","0","1280x800x24"],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True); time.sleep(1)
class T:
    def __init__(self, binary, cmd, env=None):
        ensure_x()
        e=dict(os.environ); e.update(env or {})
        self.p=subprocess.Popen([binary,"-e",*cmd],env=e,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
        for _ in range(60):
            time.sleep(0.25)
            ids=sh("xdotool","search","--pid",str(self.p.pid)).split()
            if ids: self.wid=ids[-1]; break
        time.sleep(2.0)
        g=sh("xdotool","getwindowgeometry","--shell",self.wid)
        d=dict(l.split("=") for l in g.split())
        self.x,self.y,self.w,self.h=int(d["X"]),int(d["Y"]),int(d["WIDTH"]),int(d["HEIGHT"])
        sh("xdotool","windowfocus","--sync",self.wid)
    def close(self):
        try: os.killpg(self.p.pid, signal.SIGKILL)
        except Exception: pass
        time.sleep(0.5)
    def focus(self): sh("xdotool","windowfocus","--sync",self.wid)
    def move(self,c,r,dx=0.5):
        sh("xdotool","mousemove","--window",self.wid,str(int((c+dx)*CW)),str(int(r*CH+CH/2))); time.sleep(0.04)
    def drag(self,c0,r0,c1,r1,mods=(),button=1,steps=12,dx0=0.5,dx1=0.5):
        for m in mods: sh("xdotool","keydown",m)
        self.move(c0,r0,dx0); time.sleep(0.2); sh("xdotool","mousedown",str(button)); time.sleep(0.1)
        for i in range(1,steps+1):
            self.move(c0+(c1-c0)*i/steps, r0+(r1-r0)*i//steps, dx1 if i==steps else 0.5); time.sleep(0.04)
        time.sleep(0.3); sh("xdotool","mouseup",str(button)); time.sleep(0.3)
        for m in mods: sh("xdotool","keyup",m)
    def click(self,c,r,n=1,button=1,mods=()):
        self.move(c,r); time.sleep(0.2)
        for m in mods: sh("xdotool","keydown",m)
        for _ in range(n): sh("xdotool","click",str(button)); time.sleep(0.09)
        for m in mods: sh("xdotool","keyup",m)
        time.sleep(0.4)
    def type(self,s,delay=25): self.focus(); sh("xdotool","type","--delay",str(delay),s); time.sleep(0.3)
    def key(self,*k): self.focus(); sh("xdotool","key",*k); time.sleep(0.4)
def setclip(data, sel="clipboard"):
    p=subprocess.Popen(["xclip","-i","-selection",sel],stdin=subprocess.PIPE,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); p.communicate(data if isinstance(data,bytes) else data.encode()); time.sleep(0.2)
def clip(sel="clipboard"): return subprocess.run(["xclip","-o","-selection",sel],capture_output=True).stdout
FONT="/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"
def record(name, binary, label, cmd, actions, env=None, color="red", crop=None):
    t=T(binary, cmd, env)
    raw=f"/tmp/rec/_{name}.mp4"
    ff=subprocess.Popen(["ffmpeg","-y","-loglevel","error","-f","x11grab","-draw_mouse","1","-framerate","12",
        "-video_size",f"{t.w}x{t.h}","-i",f":99+{t.x},{t.y}","-pix_fmt","yuv420p",raw],stdin=subprocess.PIPE)
    time.sleep(0.8)
    try: actions(t)
    finally:
        time.sleep(1.5); ff.communicate(b"q"); t.close()
    out=f"/tmp/rec/gifs/{name}.gif"
    lbl=label.replace(":", r"\:").replace("'", "")
    pre_vf = f"crop=iw:{crop}:0:0," if crop else ""
    vf=(pre_vf+f"pad=iw:ih+34:0:34:color=0x202020,drawtext=fontfile={FONT}:text='{lbl}':x=10:y=8:fontsize=18:fontcolor={color},"
        "fps=10,split[a][b];[a]palettegen=max_colors=48:stats_mode=diff[p];[b][p]paletteuse=dither=none:diff_mode=rectangle")
    subprocess.run(["ffmpeg","-y","-loglevel","error","-i",raw,"-vf",vf,out])
    print(name, os.path.getsize(out)//1024, "KB")
MAIN="/tmp/aud/toastty-main"
def B(branch): return f"/tmp/bins/stack-{branch}"
def pair(name, branch, cmd, actions, env=None, pre=None, crop=None):
    for tag,binary,label,color in (("before",MAIN,"BEFORE  (main @ f50f7f5)","0xff6060"),("after",B(branch),f"AFTER  (fix/{branch})","0x60ff60")):
        if pre: pre()
        record(f"{name}-{tag}", binary, label, cmd, actions, env, color, crop)
