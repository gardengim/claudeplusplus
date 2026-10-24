"""Starts claude++ in a pty and prints first-paint time, idle RSS/CPU, and key-to-screen latency.
Usage: python3 bench/pty_idle.py target/release/claudeplusplus   (no model calls)"""
import os,pty,time,subprocess,select,struct,fcntl,termios,signal,sys
binp=sys.argv[1]
pid,fd=pty.fork()
if pid==0:
    os.environ['TERM']='xterm-256color'; os.execv(binp,[binp])
fcntl.ioctl(fd,termios.TIOCSWINSZ,struct.pack('HHHH',50,160,0,0))
t0=time.time(); first=None; total=0
def drain(t):
    global first,total
    end=time.time()+t
    while time.time()<end:
        r,_,_=select.select([fd],[],[],0.05)
        if r:
            try:d=os.read(fd,65536)
            except OSError:return
            if d and first is None: first=time.time()-t0
            total+=len(d)
def ps(p): 
    o=subprocess.run(['ps','-o','rss=,time=,pid=','-p',str(p)],capture_output=True,text=True).stdout.split(); return o
def kids(p):
    o=subprocess.run(['pgrep','-P',str(p)],capture_output=True,text=True).stdout.split(); return o
drain(6)
print('first paint after %.3fs'%first)
a=ps(pid); print('ui rss KB',a[0],'cputime',a[1]); 
bytes0=total; drain(20)
b=ps(pid); print('after 20s idle: rss',b[0],'cputime',b[1],'bytes written in 20s',total-bytes0)
for k in kids(pid):
    print('child',ps(int(k)))
# typing latency: send a key, time to echo
t=time.time(); os.write(fd,b'x'); 
end=time.time()+2; lat=None
while time.time()<end:
    r,_,_=select.select([fd],[],[],0.001)
    if r:
        os.read(fd,65536); lat=time.time()-t; break
print('keystroke->first byte ms',None if lat is None else round(lat*1000,2))
os.kill(pid,signal.SIGTERM)
