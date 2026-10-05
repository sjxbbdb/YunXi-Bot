---
license: apache-2.0
base_model: intfloat/multilingual-e5-small
library_name: sentence-transformers
pipeline_tag: feature-extraction
widget:
  - source_sentence: "query: Hi, we were billed twice for March. Please refund the duplicate today."
    sentences:
      - "passage: What does the user want? invoices, payments, refunds"
      - "passage: What does the user want? bugs, outages, errors"
      - "passage: What does the user want? pricing, new contracts"
model-index:
  - name: verdict-small
    results:
      - task: {type: text-classification, name: intent classification (zero-shot, held out)}
        dataset: {name: Banking77, type: mteb/banking77, split: test}
        metrics:
          - {type: accuracy, value: 0.556, name: accuracy (zero-shot)}
          - {type: accuracy, value: 0.842, name: accuracy (16 labels/class, logistic head)}
      - task: {type: text-classification, name: sentiment (score 1-5, zero-shot, held out)}
        dataset: {name: SST-5, type: SetFit/sst5, split: test}
        metrics:
          - {type: accuracy, value: 0.403}
      - task: {type: text-classification, name: toxicity (check, zero-shot, held out)}
        dataset: {name: ToxicChat, type: lmsys/toxic-chat, split: test}
        metrics:
          - {type: roc_auc, value: 0.892, name: AUROC (zero-shot)}
          - {type: roc_auc, value: 0.939, name: AUROC (16 labels/class)}
      - task: {type: text-classification, name: typed-decisions (Jev-labelled, fine-tuned on train split)}
        dataset: {name: typed-decisions, type: LocalLLaMA/typed-decisions, split: test}
        metrics:
          - {type: accuracy, value: 0.689, name: accuracy (verdict-typed-small)}
language: [multilingual, en, hi, de, fr, es, ja, zh, ar, pt, ru, tr, ko, sw, bn]
tags: [verdict, decision-model, system-one, jev, laya, classification, calibration, onnx, transformers.js, sentence-transformers]
---

# verdict-small

Try it in the browser: https://huggingface.co/spaces/Manav2op/verdict · Colab: https://colab.research.google.com/github/Manavarya09/verdict/blob/main/examples/verdict_quickstart.ipynb

The default encoder of [Verdict](https://github.com/Manavarya09/verdict): small, fast, honest
decision models. `multilingual-e5-small` (118M) fine-tuned on a typed-decision mix of 14
public datasets (intent, NLI, ordinal reviews, safety) so that `cosine(input, option) × 20`
is a good logit over a question's options. Banking77, SST-5 and ToxicChat were **never in
the mix**; they are the held-out zero-shot numbers below.

```python
pip install verdictml
from verdict import Verdict
v = Verdict()                      # loads this model
v.choose("Billed twice, refund or we cancel", ["billing", "technical", "sales"])
v.check("Can I talk to a person?", claim="the user asks for a human")
```

Also runs in the browser via transformers.js (`onnx/model_quantized.onnx`, int8, 118 MB):

```js
const extractor = await pipeline("feature-extraction", "Manav2op/verdict-small", { dtype: "q8" });
```

## Held-out zero-shot (full test sets)

| suite | base e5-small | verdict-small |
|---|---|---|
| Banking77 (3,076) accuracy | 0.594 | 0.556 |
| SST-5 (2,210) accuracy | 0.274 | **0.403** |
| ToxicChat (5,083) AUROC | 0.59 | **0.892** |

With 16 labels per class and the package's heads: Banking77 0.842, ToxicChat AUROC 0.939.
Every number reproduces with `python -m bench.run` in the repo; protocol and all rows in
[docs/BENCHMARKS.md](https://github.com/Manavarya09/verdict/blob/main/docs/BENCHMARKS.md).

Training: `python -m train.train` (repo), 2,000 steps, batch 32, lr 2e-5, Apple M5.
Data mix and caps: `train/data.py`. Prefixes: `query: ` for inputs, `passage: ` for options.
