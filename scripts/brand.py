"""Original geometric product mark and catalog artwork; no third-party imagery."""
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[1]
for folder in ['assets', 'ui/brand', 'src-tauri/icons']:
    (ROOT / folder).mkdir(parents=True, exist_ok=True)

def icon(size=1024):
    im=Image.new('RGBA',(1024,1024),(0,0,0,0));d=ImageDraw.Draw(im)
    d.rounded_rectangle((0,0,1023,1023),radius=220,fill='#18271b')
    d.rounded_rectangle((50,50,974,974),radius=180,outline='#3e5232',width=5)
    d.line([(394,266),(195,512),(394,758)],fill='#c4ef87',width=77,joint='curve')
    d.line([(630,266),(829,512),(630,758)],fill='#c4ef87',width=77,joint='curve')
    d.rounded_rectangle((302,480,722,544),radius=30,fill='#c4ef87')
    d.polygon([(512,321),(556,467),(703,512),(556,557),(512,703),(468,557),(321,512),(468,467)],fill='#e8f8d9')
    return im.resize((size,size),Image.Resampling.LANCZOS)

im=icon();im.save(ROOT/'assets/icon.png');im.save(ROOT/'ui/brand/icon.png')
for size in [32,128]:icon(size).save(ROOT/f'src-tauri/icons/{size}x{size}.png')
icon(256).save(ROOT/'src-tauri/icons/icon.ico',sizes=[(x,x) for x in [16,24,32,48,64,128,256]])

def font(n,bold=False): return ImageFont.truetype('C:/Windows/Fonts/segoeuib.ttf' if bold else 'C:/Windows/Fonts/segoeui.ttf',n)
art=Image.new('RGB',(1600,900),'#111915');d=ImageDraw.Draw(art)
d.rounded_rectangle((55,55,1545,845),radius=30,outline='#34432f',width=2)
mark=icon(110);art.paste(mark,(110,110),mark)
d.text((255,120),'DGXBRIDGE',font=font(32,True),fill='#c4ef87')
d.text((255,168),'BY DELORESAI',font=font(18),fill='#93a78d')
d.text((110,282),'Room to build.',font=font(90),fill='#edf3e8')
d.text((116,401),'Your laptop. Your Spark.',font=font(32),fill='#abbda3')
d.text((116,447),'One job board for your coding agents.',font=font(32),fill='#abbda3')
for box,label,sub in [((850,220,1200,425),'LAPTOP','Windows builds'),((1060,520,1410,725),'SPARK','Linux & heavy compute')]:
    d.rounded_rectangle(box,radius=24,fill='#22301e',outline='#657d4f',width=3)
    d.text((box[0]+30,box[1]+40),label,font=font(24,True),fill='#c4ef87')
    d.text((box[0]+30,box[1]+97),sub,font=font(23),fill='#e3eddc')
d.line([(1025,425),(1025,473),(1235,473),(1235,520)],fill='#c4ef87',width=4)
for x,y in [(1025,425),(1235,520)]:d.ellipse((x-7,y-7,x+7,y+7),fill='#c4ef87')
d.text((116,728),'CLAUDE CODE  /  CODEX  /  YOU',font=font(19),fill='#93a78d')
art.save(ROOT/'assets/thumbnail.png')
