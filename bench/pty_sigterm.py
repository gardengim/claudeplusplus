"""Checks that SIGTERM leaves the terminal restored (mouse reporting off, alternate screen left).
Usage: python3 bench/pty_sigterm.py target/release/claudeplusplus"""
import os,pty,time,select,signal,struct,fcntl,termios,sys
binp=sys.argv[1]
pid,fd=pty.fork()
if pid==0:
    os.environ['TERM']='xterm-256color'; os.execv(binp,[binp])
fcntl.ioctl(fd,termios.TIOCSWINSZ,struct.pack('HHHH',40,120,0,0))
buf=b''
def drain(t):
    global buf
    end=time.time()+t
    while time.time()<end:
        r,_,_=select.select([fd],[],[],0.05)
        if r:
            try:d=os.read(fd,65536)
            except OSError:return
            if not d:return
            buf+=d
drain(2)
assert b'\x1b[?1000h' in buf, 'mouse on'
os.kill(pid,signal.SIGTERM); drain(3)
_,st=os.waitpid(pid,0)
print('exit',st, 'mouse off sent:', b'\x1b[?1000l' in buf, 'alt screen left:', b'\x1b[?1049l' in buf)
