"""本地化工具（#87）。
  python scripts/i18n.py extract   从 ui/*.slint 与 src/**/*.rs 提取待翻译文案，更新 crates/app/lang 下的模板与译文表
  python scripts/i18n.py check     检查英文翻译是否覆盖全部文案（漏翻译则退出码 1）
Slint 的 @tr("…") 以中文原文作 key；Rust 里用 tr("…") 同样以中文原文作 key（译文表 en-rust.json）。
"""
import re, sys, glob, os, json

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..')
APP = os.path.join(ROOT, 'crates', 'app')
BS = chr(92)
NL = chr(10)
STR = '"((?:[^"' + BS + BS + NL + ']|' + BS + BS + '.)*)"'


def unesc(s):
    out = []
    i = 0
    while i < len(s):
        c = s[i]
        if c == BS and i + 1 < len(s):
            n = s[i + 1]
            out.append({'n': '\n', '"': '"', BS: BS}.get(n, BS + n))
            i += 2
        else:
            out.append(c)
            i += 1
    return ''.join(out)


def esc(s):
    return s.replace(BS, BS + BS).replace('"', BS + '"').replace('\n', BS + 'n')


def slint_keys():
    keys = {}
    for f in sorted(glob.glob(os.path.join(APP, 'ui', '*.slint'))):
        t = open(f, encoding='utf-8').read()
        for m in re.finditer(r'@tr\(\s*' + STR, t):
            line = t.count('\n', 0, m.start()) + 1
            keys.setdefault(unesc(m.group(1)), []).append('ui/%s:%d' % (os.path.basename(f), line))
    return keys


def rust_keys():
    keys = {}
    for f in sorted(glob.glob(os.path.join(APP, 'src', '**', '*.rs'), recursive=True)):
        if f.endswith('screenshot.rs') or f.endswith('i18n.rs'):
            continue
        t = open(f, encoding='utf-8').read()
        for m in re.finditer(r'(?<![A-Za-z0-9_])trf?\(\s*' + STR, t):
            line = t.count('\n', 0, m.start()) + 1
            rel = os.path.relpath(f, os.path.join(APP, 'src')).replace(BS, '/')
            keys.setdefault(unesc(m.group(1)), []).append('src/%s:%d' % (rel, line))
    return keys


def read_po(path):
    out = {}
    if not os.path.exists(path):
        return out
    mid = mstr = mode = None
    for line in open(path, encoding='utf-8').read().splitlines():
        if line.startswith('msgid '):
            if mid is not None and mstr is not None:
                out[mid] = mstr
            mid = unesc(re.match(r'msgid ' + STR, line).group(1))
            mstr = None
            mode = 'id'
        elif line.startswith('msgstr '):
            mstr = unesc(re.match(r'msgstr ' + STR, line).group(1))
            mode = 'str'
        elif line.startswith('"'):
            v = unesc(re.match(STR, line).group(1))
            if mode == 'id':
                mid += v
            elif mode == 'str':
                mstr += v
    if mid is not None and mstr is not None:
        out[mid] = mstr
    return out


def write_po(path, keys, trans):
    lines = ['msgid ""', 'msgstr ""', '"Content-Type: text/plain; charset=UTF-8' + BS + 'n"',
             '"Language: en' + BS + 'n"', '"Plural-Forms: nplurals=2; plural=(n != 1);' + BS + 'n"', '']
    for k in sorted(keys):
        lines.append('#: ' + ' '.join(keys[k][:3]))
        lines.append('msgid "%s"' % esc(k))
        lines.append('msgstr "%s"' % esc(trans.get(k, '')))
        lines.append('')
    open(path, 'w', encoding='utf-8', newline='\n').write('\n'.join(lines))


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else 'check'
    lang = os.path.join(APP, 'lang')
    po = os.path.join(lang, 'en', 'LC_MESSAGES', 'tml-app.po')
    rj = os.path.join(lang, 'en-rust.json')
    sk, rk = slint_keys(), rust_keys()
    if cmd == 'extract':
        os.makedirs(os.path.dirname(po), exist_ok=True)
        write_po(os.path.join(lang, 'tml-app.pot'), sk, {})
        write_po(po, sk, read_po(po))
        old = json.load(open(rj, encoding='utf-8')) if os.path.exists(rj) else {}
        json.dump({k: old.get(k, '') for k in sorted(rk)}, open(rj, 'w', encoding='utf-8', newline='\n'),
                  ensure_ascii=False, indent=1)
        print('Slint %d 条，Rust %d 条' % (len(sk), len(rk)))
        return 0
    bad = []
    t = read_po(po)
    bad += [('slint', k) for k in sk if not t.get(k)]
    r = json.load(open(rj, encoding='utf-8')) if os.path.exists(rj) else {}
    bad += [('rust', k) for k in rk if not r.get(k)]
    for kind, k in bad:
        print('缺英文翻译 [%s] %s' % (kind, k))
    for k in r:
        if k not in rk:
            print('多余的 Rust 翻译 ' + k)
    print('Slint %d 条，Rust %d 条，缺 %d 条' % (len(sk), len(rk), len(bad)))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
