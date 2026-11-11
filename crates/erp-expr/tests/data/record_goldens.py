import builtins
import datetime
import json
import sys
import time

from dateutil.relativedelta import relativedelta

src, exprs = sys.stdin.read().split("=====EXPRS=====\n", 1)
ns = {}
exec(compile(src, "commands.py", "exec"), ns)
Command = ns["Command"]

ctx_base = {
    "__builtins__": builtins,
    "Command": Command,
    "time": time,
    "datetime": datetime.datetime,
    "DateTime": datetime.datetime,
    "timedelta": datetime.timedelta,
    "relativedelta": relativedelta,
}

out = []
for line in exprs.split("\n"):
    if not line.strip():
        continue
    expr = line.encode("utf-8").decode("unicode_escape") if line.startswith("'''multi") else line
    ctx = dict(ctx_base)
    try:
        v = eval(expr, ctx)
        if isinstance(v, (type({}.keys()), type({}.values()), type({}.items()))):
            v = list(v)
        if isinstance(v, Command):
            v = int(v)
        out.append([expr, "ok", repr(v)])
    except SyntaxError as e:
        out.append([expr, "syntax", e.msg])
    except Exception as e:
        out.append([expr, "err", f"{type(e).__name__}: {e}"])
print(json.dumps(out, ensure_ascii=False, indent=0))
