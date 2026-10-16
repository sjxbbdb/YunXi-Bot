#!/usr/bin/env python3
"""复用候选模型断点下载器；只从固定版本与 SHA256 清单准备向量模型。"""
import argparse
from pathlib import Path
from fetch_model import load_manifest, REPO_ROOT
from fetch_mlx_candidates import download


def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--home',type=Path,default=REPO_ROOT/'data')
    ap.add_argument('--only',choices=['multilingual-e5-small','embeddinggemma-2'])
    args=ap.parse_args()
    names=[args.only] if args.only else ['multilingual-e5-small','embeddinggemma-2']
    for e in load_manifest()['models']:
        if e['name'] not in names: continue
        files=[dict(path=n,size=e['file_sizes'][n],sha256=e['sha256_files'][n]) for n in e['required_files']]
        download(dict(repo_id=e['source_repo'],revision=e['revision'],files=files,
                      bytes=sum(f['size'] for f in files)),args.home)


if __name__=='__main__': main()
