#!/usr/bin/env python3
"""EmbeddingGemma 2 文本编码服务：回环、离线、CPU、小批量串行；不持有私人资料。"""
import argparse
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from embedding_backend import Encoder
from scripts.fetch_model import default_home


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def setup(self):
        super().setup()
        self.connection.settimeout(30)

    def send(self, status, value):
        raw = json.dumps(value, ensure_ascii=False, allow_nan=False).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        if self.path != '/health':
            return self.send(404, {'error': 'unknown endpoint'})
        self.send(200, {'status': 'ok', 'model': self.server.encoder.model_id, 'dimension': 768, 'device': 'cpu'})

    def do_POST(self):
        if self.path != '/embed':
            return self.send(404, {'error': 'unknown endpoint'})
        try:
            size = int(self.headers.get('Content-Length', '0'))
            if self.headers.get('Transfer-Encoding') or not 0 < size <= 65536:
                raise ValueError('invalid body size')
            raw = self.rfile.read(size)
            if len(raw) != size:
                raise ValueError('incomplete body')
            texts, kind = validate(json.loads(raw))
            vectors = self.server.encoder.encode(texts, kind)
            self.send(200, {'model': self.server.encoder.model_id, 'embeddings': vectors})
        except (ValueError, TypeError, UnicodeError):
            self.send(400, {'error': 'invalid request; check texts, kind and token limit'})
        except Exception as exc:
            self.send(503, {'error': f'encoder failed: {type(exc).__name__}'})


def validate(value):
    if not isinstance(value, dict) or set(value) != {'texts', 'kind'}:
        raise ValueError('expected texts and kind')
    texts, kind = value['texts'], value['kind']
    if kind not in ('query', 'passage') or not isinstance(texts, list) or not 1 <= len(texts) <= 8:
        raise ValueError('invalid kind or batch size')
    if any(not isinstance(t, str) or not t.strip() or len(t) > 2000 for t in texts):
        raise ValueError('invalid text')
    return texts, kind


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--port', type=int, default=17873)
    ap.add_argument('--model-dir', type=Path, default=default_home() / 'models/embeddinggemma-2')
    args = ap.parse_args()
    with HTTPServer(('127.0.0.1', args.port), Handler) as server:
        # 先占用端口，避免两个 CLI 同时启动时重复加载权重。
        encoder = Encoder('embeddinggemma-2', args.model_dir)
        server.encoder = encoder
        print(f'[embedding] ready {encoder.model_id}, cpu, port={args.port}', flush=True)
        server.serve_forever()


if __name__ == '__main__':
    main()
