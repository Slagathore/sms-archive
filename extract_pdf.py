import sys
try:
    import PyPDF2
except Exception as e:
    print('PyPDF2 not available', e)
    sys.exit(0)
from pathlib import Path
pdf=Path('docs/Fields in XML backup files – SyncTech.pdf')
reader = PyPDF2.PdfReader(str(pdf))
for i,p in enumerate(reader.pages):
    text = p.extract_text() or ''
    print(f"--- page {i+1} ---")
    print(text)
