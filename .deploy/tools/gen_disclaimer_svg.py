#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生成免责声明的 SVG 横幅 (GitHub 的 Markdown 不支持给文字上色, 只能走图片)。"""
import os
import sys

OUT = sys.argv[1] if len(sys.argv) > 1 else 'docs/disclaimer.svg'

LINES = [
    ("远界的开发与维护者", "不纵容、不支持", "任何不道德或非法的软件使用行为。"),
    ("未经授权访问他人设备、擅自控制他人计算机、侵犯隐私、窃取数据、破坏系统等行为，", None, None),
    ("属于违法行为，远界不提供用于此类场景的支持；相关法律责任由行为人依法承担。", None, None),
    ("", None, None),
    ("本项目仅可用于", "合法的自有设备运维", "、", "经授权的远程协助", "、", "内网办公", "等正当场景。"),
    ("使用者对自己使用本项目的一切行为负责；因不当使用造成的一切后果由使用者自行承担，", None, None),
    ("与本项目的开发与维护者无关。", None, None),
]

W = 940
PAD = 34
TITLE_Y = 66
LINE_H = 32
FIRST_Y = 118
BOX_H = FIRST_Y + len(LINES) * LINE_H + 18

FONT = "'Microsoft YaHei','PingFang SC','Hiragino Sans GB','Source Han Sans SC','Noto Sans CJK SC',sans-serif"

def esc(s):
    return (s.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;'))

parts = []
parts.append(
    '<svg xmlns="http://www.w3.org/2000/svg" width="%d" height="%d" '
    'viewBox="0 0 %d %d" role="img" aria-label="免责声明">' % (W, BOX_H, W, BOX_H))
parts.append(
    '<rect x="1" y="1" width="%d" height="%d" rx="12" fill="#fef2f2" '
    'stroke="#dc2626" stroke-width="2"/>' % (W - 2, BOX_H - 2))
parts.append(
    '<rect x="1" y="1" width="7" height="%d" rx="3" fill="#dc2626"/>' % (BOX_H - 2))
parts.append(
    '<text x="%d" y="%d" font-family="%s" font-size="30" font-weight="700" fill="#b91c1c">'
    '⚠ 免责声明</text>' % (PAD + 10, TITLE_Y, FONT))

y = FIRST_Y
for line in LINES:
    if not any(line):
        y += LINE_H
        continue
    x = PAD + 10
    for seg in line:
        if seg is None:
            continue
        if seg == '不纵容、不支持' or seg == '合法的自有设备运维' or seg == '经授权的远程协助' or seg == '内网办公':
            parts.append(
                '<text x="%d" y="%d" font-family="%s" font-size="19" font-weight="700" '
                'fill="#b91c1c">%s</text>' % (x, y, FONT, esc(seg)))
        else:
            parts.append(
                '<text x="%d" y="%d" font-family="%s" font-size="19" fill="#7f1d1d">%s</text>'
                % (x, y, FONT, esc(seg)))
        # 中文字宽约等于字号 (1 em), 再留 3px 间隙
        x += int(len(seg) * 19) + 3
    y += LINE_H

parts.append('</svg>')

os.makedirs(os.path.dirname(OUT) or '.', exist_ok=True)
with open(OUT, 'w', encoding='utf-8', newline='\n') as f:
    f.write('\n'.join(parts))
print('已生成 %s (%d 字节)' % (OUT, os.path.getsize(OUT)))
