"""固定版本的候选编码器；统一精度/池化/设备，仅保留官方要求的输入前缀差异。"""
from pathlib import Path
import math
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scripts.fetch_model import load_manifest, model_ready

MODELS = {
    'multilingual-e5-small': dict(dimension=384, max_tokens=512,
        query='query: ', passage='passage: '),
    'embeddinggemma-2': dict(dimension=768, max_tokens=8192,
        query='task: search result | query: ', passage='title: none | text: '),
}


class Encoder:
    def __init__(self, name, directory, device='cpu'):
        import torch
        from transformers import AutoConfig, AutoModel, AutoTokenizer
        self.name = name
        self.spec = MODELS[name]
        entry = next(e for e in load_manifest()['models'] if e['name'] == name)
        if not model_ready(directory, entry['required_files'], entry['sha256_files']):
            raise ValueError(f'{name}: missing weights or checksum mismatch')
        self.model_id = f"{entry['source_repo']}@{entry['revision']}"
        torch.set_num_threads(4)
        torch.manual_seed(0)
        self.torch, self.device = torch, device
        self.tokenizer = AutoTokenizer.from_pretrained(str(directory), local_files_only=True, trust_remote_code=False)
        cfg = AutoConfig.from_pretrained(str(directory), local_files_only=True, trust_remote_code=False)
        if name == 'embeddinggemma-2':
            cfg.vision_config = cfg.audio_config = None
        model, loading = AutoModel.from_pretrained(str(directory), config=cfg,
            local_files_only=True, trust_remote_code=False, dtype=torch.float32,
            attn_implementation='sdpa', output_loading_info=True)
        if any(loading.get(key) for key in ('missing_keys', 'mismatched_keys', 'error_msgs')):
            raise ValueError(f'{name}: incomplete model initialization')
        self.model = model.eval().to(device)
        if name == 'embeddinggemma-2':
            assert self.model.vision_tower is None and self.model.audio_tower is None
        self.parameters = sum(p.numel() for p in self.model.parameters())

    def encode(self, texts, kind):
        if kind not in ('query', 'passage') or not 1 <= len(texts) <= 8:
            raise ValueError('invalid kind or batch size')
        torch = self.torch
        batch = self.tokenizer([self.spec[kind] + t for t in texts], padding=True,
                               truncation=False, return_tensors='pt')
        if batch['input_ids'].shape[1] > self.spec['max_tokens']:
            raise ValueError('input exceeds model token limit; no silent truncation')
        batch = {k: v.to(self.device) for k,v in batch.items()}
        with torch.inference_mode():
            hidden = self.model(**batch).last_hidden_state.float()
            mask = batch['attention_mask'].unsqueeze(-1)
            vectors = (hidden * mask).sum(dim=1) / mask.sum(dim=1)
            vectors = torch.nn.functional.normalize(vectors, p=2, dim=1).cpu().tolist()
        if any(len(v) != self.spec['dimension'] or not all(math.isfinite(x) for x in v) for v in vectors):
            raise ValueError('invalid embedding output')
        return vectors
