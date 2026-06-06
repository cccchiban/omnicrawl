import urllib.request, json, sys
url='https://wttr.in/Shishi?format=%C+%t&lang=zh'
try:
 f=urllib.request.urlopen(url,timeout=10)
 print(f.read().decode())
except Exception as e:
 print('error:'+str(e),file=sys.stderr)