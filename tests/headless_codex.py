#!/usr/bin/env python3
import sys,json,os
scenario=os.environ.get('CX_TEST_SCENARIO','success')
def send(v): print(json.dumps(v),flush=True)
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); ident=m.get('id'); p=m.get('params',{})
 if method=='initialize': send({'id':ident,'result':{}})
 elif method=='account/login/start':
  assert p['chatgptAccountId']=='bot'
  if scenario=='auth':
   send({'id':ident,'error':{'message':'fixture-secret'}})
   continue
  send({'id':ident,'result':{'type':'chatgptAuthTokens'}})
 elif method in ['thread/start','thread/resume']:
  if scenario=='resume-baseline':
   send({'method':'thread/tokenUsage/updated','params':{'threadId':'headless-root','turnId':'old-turn','tokenUsage':{'total':{'inputTokens':100,'cachedInputTokens':40,'outputTokens':20,'reasoningOutputTokens':10,'totalTokens':120},'last':{}}}})
  if scenario=='missing-evidence':
   send({'id':ident,'result':{'thread':{'id':'headless-root'}}})
   continue
  send({'id':ident,'result':{'thread':{'id':'headless-root'},'model':'wrong-model' if scenario=='mismatch' else p.get('model','model-test'),'reasoningEffort':p.get('config',{}).get('model_reasoning_effort','high')}})
 elif method=='turn/start':
  if scenario=='orphan-tool':
   import subprocess
   program = """import signal,time,os
from pathlib import Path
signal.signal(signal.SIGTERM,signal.SIG_IGN)
p=Path(os.environ['CX_TEST_MARKER'])
p.with_suffix('.pid').write_text(str(os.getpid()))
with p.open('a') as f:
 while True:
  f.write('x');f.flush();time.sleep(0.02)
"""
   subprocess.Popen([sys.executable,'-c',program])
   continue
  if scenario=='stdin': assert p['input'][0]['text']=='stdin prompt'
  if scenario=='approval':
   send({'id':'approval-1','method':'item/commandExecution/requestApproval','params':{}})
   continue
  send({'id':ident,'result':{'turn':{'id':'turn-1'}}})
  send({'method':'turn/started','params':{'threadId':'headless-root','turn':{'id':'turn-1'}}})
  send({'method':'item/completed','params':{'threadId':'headless-root','turnId':'turn-1','item':{'id':'answer','type':'agentMessage','text':'hi'}}})
  send({'method':'thread/tokenUsage/updated','params':{'threadId':'headless-root','turnId':'turn-1','tokenUsage':{'total':{'inputTokens':110 if scenario=='resume-baseline' else 10,'cachedInputTokens':44 if scenario=='resume-baseline' else 4,'outputTokens':22 if scenario=='resume-baseline' else 2,'reasoningOutputTokens':11 if scenario=='resume-baseline' else 1,'totalTokens':132 if scenario=='resume-baseline' else 12},'last':{}}}})
  error={'message':'fixture-secret','codexErrorInfo':'usageLimitExceeded'} if scenario=='quota' else None
  send({'method':'turn/completed','params':{'threadId':'headless-root','turn':{'id':'turn-1','status':'failed' if error else 'completed','error':error}}})
