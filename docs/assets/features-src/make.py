"""Writes the README's feature images' sources (one HTML file each). Render: sh render.sh"""
import os
here = os.path.dirname(os.path.abspath(__file__))
V = '../vendors/'
def mk(m, px=18):
    src = {'anthropic': V+'anthropic-dark.svg', 'openai': V+'openai-dark.svg', 'google': V+'gemini.svg',
           'deepseek': V+'deepseek.svg', 'mistral': V+'mistral.svg', 'xai': V+'xai-dark.svg', 'mcp': V+'mcp-dark.svg',
           'respan': V+'respan-dark.svg'}[m]
    return f'<img src="{src}" style="width:{px}px;height:{px}px;flex:none">'

HEAD = """<!doctype html><html><head><meta charset="utf-8"><style>
* { box-sizing: border-box; margin: 0; }
html { width: 800px; height: 400px; } body { width: 615px; height: 308px; overflow: hidden; background: transparent; }
body { zoom: 1.3; color: #f4f4f6; -webkit-font-smoothing: antialiased; font: 15px/1.4 Inter, -apple-system, "SF Pro Text", "Segoe UI", Helvetica, sans-serif; }
.panel { position: absolute; inset: 0; border-radius: 22px; background: #0b0b0f; box-shadow: inset 0 0 0 1px #24242a; overflow: hidden; padding: 18px 22px; display: flex; flex-direction: column; justify-content: center; }
.panel::before { content: ""; position: absolute; inset: 0; background: radial-gradient(ellipse 70% 60% at 50% 0%, rgba(143,156,255,.08), transparent 70%); pointer-events: none; }
.mono { font-family: "JetBrains Mono", ui-monospace, "SF Mono", Menlo, monospace; letter-spacing: -.2px; }
.lab { font-size: 12px; letter-spacing: .1em; text-transform: uppercase; color: #6b6d76; }
.box { background: #131317; border-radius: 14px; box-shadow: inset 0 0 0 1px #26262c; }
.row { display: flex; align-items: center; gap: 10px; }
.ok { color: #31de4b; } .bad { color: #f55656; } .dim { color: #6b6d76; } .acc { color: #8f9cff; } .mid { color: #b1b3bc; }
@@CSS@@
</style></head><body><div class="panel">@@BODY@@</div></body></html>"""

CARDS = {}

CARDS['any-sdk'] = ("""
.code { padding: 14px 18px; font-size: 16px; line-height: 1.65; }
.k { color: #8f9cff; } .s { color: #e8c38a; }
.flow { margin-top: 12px; display: flex; align-items: center; gap: 12px; }
.chip { padding: 9px 14px; border-radius: 999px; font-size: 14px; }
.reply { margin-top: 12px; padding: 12px 16px; font-size: 16px; }
""", f"""
<div class="box code mono"><div class="lab" style="margin-bottom:10px">Anthropic SDK</div>
<span class="k">client</span>.messages.create(<br>
&nbsp;&nbsp;model=<span class="s">"gpt-6-luna"</span>, …)</div>
<div class="flow"><span class="box chip mono row">{mk('anthropic',16)} messages</span><span class="dim">→</span>
<span class="box chip mono row">{mk('respan',16)} translate</span><span class="dim">→</span>
<span class="box chip mono row">{mk('openai',16)} chat</span></div>
<div class="box reply row">{mk('openai',20)}<span>Hello! How can I help today?</span></div>
""")

def wave(n, h=46):
    import math, random
    random.seed(n * 7 + h)
    return ''.join(f'<i style="height:{6 + abs(math.sin(i * .45)) * (.35 + random.random() * .65) * h:.0f}px"></i>' for i in range(n))
CARDS['multimodal'] = ("""
.grid { display: grid; grid-template-columns: 1fr 1fr; grid-template-rows: minmax(0, 1fr) minmax(0, 1fr); gap: 10px; flex: 1; min-height: 0; }
.t { padding: 12px 14px; position: relative; overflow: hidden; }
.w { display: flex; align-items: center; gap: 3px; height: 46px; margin-top: 8px; } .w i { width: 4px; border-radius: 2px; background: #e6e7ec; }
.ph { padding: 0; } .ph img.p { width: 100%; height: 100%; object-fit: cover; display: block; }
.tag { position: absolute; left: 12px; bottom: 12px; padding: 5px 11px; border-radius: 999px; background: rgba(0,0,0,.6); font-size: 13px; display: flex; gap: 8px; align-items: center; }
.frames { position: absolute; inset: 0; display: grid; grid-template-columns: repeat(3, 1fr); gap: 3px; }
.frames img { width: 100%; height: 100%; object-fit: cover; }
.play { position: absolute; left: 50%; top: 50%; width: 46px; height: 46px; margin: -23px 0 0 -23px; border-radius: 50%; background: rgba(0,0,0,.55); }
.play::before { content: ""; position: absolute; left: 17px; top: 13px; border-left: 16px solid #fff; border-top: 10px solid transparent; border-bottom: 10px solid transparent; }
.vec { font-size: 14px; color: #b1b3bc; margin-top: 8px; line-height: 1.6; }
""", f"""<div class="grid">
<div class="box t"><div class="row lab">{mk('openai',14)} realtime ⇄ {mk('google',14)} live</div><div class="w">{wave(44, 40)}</div></div>
<div class="box t ph"><img class="p" src="../hero-src/img/p6.jpg"><span class="tag">{mk('google',14)} image</span></div>
<div class="box t ph"><div class="frames"><img src="../hero-src/img/p2.jpg" style="object-position:0% 50%"><img src="../hero-src/img/p2.jpg" style="object-position:50% 50%"><img src="../hero-src/img/p2.jpg" style="object-position:100% 50%"></div><span class="play"></span><span class="tag">{mk('openai',14)} video · 00:06</span></div>
<div class="box t"><div class="row lab">{mk('mistral',14)} embeddings</div><div class="vec mono">[0.021, -0.113, 0.448,<br>&nbsp;0.207, -0.391, 0.064,<br>&nbsp;-0.158, 0.772, …]</div></div>
</div>""")

CARDS['tools'] = ("""
.msg { padding: 11px 16px; font-size: 16px; max-width: 78%; }
.u { margin-left: auto; background: #24242b; }
.steps { margin: 10px 0; display: flex; flex-direction: column; gap: 9px; }
.st { padding: 9px 14px; font-size: 14.5px; display: flex; align-items: center; gap: 10px; }
.st .n { width: 22px; height: 22px; border-radius: 50%; background: #24242b; display: grid; place-items: center; font-size: 12px; color: #b1b3bc; }
.st .ok { margin-left: auto; font-size: 13px; }
""", f"""
<div class="box msg u">Flights to Tokyo under $900?</div>
<div class="steps">
<div class="box st mono"><span class="n">1</span>{mk('mcp',16)} search__web_search(<span class="mid">"SFO→HND"</span>)<span class="ok">done</span></div>
<div class="box st mono"><span class="n">2</span>{mk('mcp',16)} browser__fetch(<span class="mid">"kayak.com/…"</span>)<span class="ok">done</span></div>
</div>
<div class="box msg row" style="gap:12px">{mk('deepseek',20)}<span>ANA, Tue 11:05, $842.</span></div>
""")

CARDS['never-down'] = ("""
.sec { padding: 13px 18px; }
.ln { display: flex; align-items: center; gap: 12px; font-size: 15px; margin-top: 8px; }
.ln .code { margin-left: auto; font-size: 14px; }
.track { position: relative; height: 30px; margin-top: 8px; }
.bar { position: absolute; height: 30px; border-radius: 9px; display: flex; align-items: center; gap: 8px; padding: 0 12px; font-size: 13.5px; }
.axis { display: flex; justify-content: space-between; margin-top: 6px; font-size: 12px; color: #6b6d76; }
""", f"""
<div class="box sec"><div class="lab">fallback</div>
<div class="ln mono">{mk('anthropic',18)} claude-opus-5-5<span class="code bad">529</span></div>
<div class="ln mono">{mk('openai',18)} gpt-6-luna<span class="code ok">200</span></div></div>
<div class="box sec" style="margin-top:10px"><div class="lab">hedge = "800ms"</div>
<div class="track"><div class="bar mono" style="left:0;width:96%;background:#1d1d23;color:#8b8d96">{mk('anthropic',16)} claude · waiting</div></div>
<div class="track"><div class="bar mono" style="left:50%;width:34%;background:rgba(49,222,75,.14);color:#31de4b">{mk('google',16)} gemini · won</div></div>
<div class="axis mono"><span>0</span><span>800ms</span><span>1.6s</span></div></div>
""")

CARDS['plugins'] = ("""
.code { padding: 18px 20px; font-size: 15px; line-height: 1.7; white-space: pre; }
.k { color: #8f9cff; } .f { color: #e8c38a; } .c { color: #6b6d76; }
.io { margin-top: 10px; display: grid; grid-template-columns: 1fr auto 1fr; gap: 12px; align-items: center; }
.io .box { padding: 10px 14px; font-size: 14.5px; }
.io .lab { font-size: 11px; margin-bottom: 8px; }
""", """
<div class="box code mono"><span class="c">// examples/recipes/pii.rs</span>
<span class="k">async fn</span> <span class="f">on_provider</span>(&amp;self, px) {
    px.<span class="f">filter</span>(|chat| <span class="f">redact</span>(chat, &amp;vault))
}</div>
<div class="io mono"><div class="box"><div class="lab">you send</div>Mail <span class="mid">bob@x.io</span></div><span class="dim">→</span><div class="box"><div class="lab">provider sees</div>Mail <span class="acc">&lt;EMAIL_1&gt;</span></div></div>
""")

CARDS['observe'] = ("""
.log { padding: 8px 16px; font-size: 13.5px; }
.lr { display: grid; grid-template-columns: 0 40px 1fr 62px 62px; gap: 10px; align-items: center; padding: 5px 0; border-bottom: 1px solid #1e1e23; }
.lr:last-child { border: 0; }
.lr .m { display: flex; align-items: center; gap: 8px; }
.lr .r { text-align: right; }
.tr { margin-top: 10px; padding: 11px 16px; }
.sp { display: grid; grid-template-columns: 130px 1fr; gap: 12px; align-items: center; font-size: 13px; margin-top: 5px; }
.sp i { display: block; height: 10px; border-radius: 5px; background: #8f9cff; }
""", f"""
<div class="box log mono">
<div class="lr dim" style="font-size:11.5px;letter-spacing:.06em"><span></span><span>CODE</span><span>MODEL</span><span class="r">LATENCY</span><span class="r">COST</span></div>
<div class="lr"><span></span><span class="ok">200</span><span class="m">{mk('anthropic',15)}claude-opus-5-5</span><span class="r">1.2s</span><span class="r">$0.042</span></div>
<div class="lr"><span></span><span class="ok">200</span><span class="m">{mk('openai',15)}gpt-6-luna</span><span class="r">476ms</span><span class="r">$0.003</span></div>
<div class="lr"><span></span><span class="bad">529</span><span class="m">{mk('anthropic',15)}claude-opus-5-5</span><span class="r">88ms</span><span class="r">$0</span></div>
</div>
<div class="box tr"><div class="row lab">{mk('respan',14)} trace · 3 spans · 1.2s</div>
<div class="sp mono"><span class="mid">request</span><i style="width:100%"></i></div>
<div class="sp mono"><span class="mid">claude-opus-5-5</span><i style="width:52%;margin-left:4%"></i></div>
<div class="sp mono"><span class="mid">tool: web_search</span><i style="width:24%;margin-left:58%"></i></div>
</div>
""")


for name, (css, body) in CARDS.items():
    open(os.path.join(here, name + '.html'), 'w').write(HEAD.replace('@@CSS@@', css).replace('@@BODY@@', body))
print(' '.join(CARDS))
