/*
 * fs32_probe.c — 32-bit fs-semantics probe, run INSIDE a sud box.
 *
 * REPRODUCES a real data-loss bug: build static-32 (zig cc -target
 * x86-linux-musl -static) and exec it in a `sarun run --sud -b` box, which
 * cross-class-execs it under sud32. It creates 250 files, then readdir/stat.
 *
 * OBSERVED (deterministic): in the OVERLAY (/root) all checks pass; in INRAMFS
 * (/tmp) readdir sees only 226 of 250 and stat() of some created files returns
 * ENOENT — i.e. files that open(O_CREAT) succeeded for become unfindable. The
 * 64-bit build of the same probe passes. So the defect is in sud32's inramfs
 * path (getdents64 resume cookie or sud_ir_dir_link block-full handling under
 * the 32-bit syscall ABI). This is the "No such file" that only a 32-bit make
 * writing intermediates to /tmp hits.
 *
 * Not yet wired into the Makefile (needs zig + a live box); kept as the
 * reproduction until the inramfs 32-bit path is fixed and this goes green.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <dirent.h>
#include <sys/stat.h>
static int fails=0;
static void P(const char*n){printf("PASS %s\n",n);}
static void F(const char*n,const char*m){printf("FAIL %s :: %s\n",n,m);fails++;}
static void run(const char*d){
  mkdir(d,0755);
  if(chdir(d)){F("chdir",d);return;}
  int N=250;
  for(int i=0;i<N;i++){char p[64];sprintf(p,"f%d",i);int fd=open(p,O_CREAT|O_WRONLY|O_TRUNC,0644);
    if(fd<0){F("create","open failed");return;}char b[32];int n=sprintf(b,"v%d",i);write(fd,b,n);close(fd);}
  DIR*dp=opendir(".");int cnt=0;struct dirent*e;
  if(!dp){F("opendir",d);return;}
  while((e=readdir(dp))){if(e->d_name[0]=='f')cnt++;}
  closedir(dp);
  if(cnt==N)P("readdir-count");else{char m[64];sprintf(m,"got=%d want=%d",cnt,N);F("readdir-count",m);}
  int rbfail=0;
  for(int i=0;i<N;i+=13){char p[64];sprintf(p,"f%d",i);struct stat st;
    if(stat(p,&st)){char m[64];sprintf(m,"stat %s ENOENT (listed but not statable!)",p);F("stat-listed",m);rbfail=1;continue;}
    int fd=open(p,O_RDONLY);if(fd<0){F("reopen","ENOENT");rbfail=1;continue;}
    char b[32]={0};read(fd,b,31);close(fd);char exp[32];sprintf(exp,"v%d",i);
    if(strcmp(b,exp)){char m[80];sprintf(m,"%s got=[%s] want=[%s]",p,b,exp);F("readback",m);rbfail=1;}}
  if(!rbfail)P("stat+readback");
  if(rename("f1","frenamed")==0 && access("frenamed",F_OK)==0 && access("f1",F_OK)!=0)P("rename");else F("rename","x");
  unlink("f2"); if(access("f2",F_OK)!=0)P("unlink-gone");else F("unlink-gone","still there");
}
int main(){printf("== fs32 in OVERLAY ==\n");run("/root/o32");
  printf("== fs32 in INRAMFS ==\n");run("/tmp/i32");
  printf("FS32-DONE fails=%d\n",fails);return fails?1:0;}
