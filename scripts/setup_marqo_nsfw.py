# Download Marqo/nsfw-image-detection-384 (timm ViT-tiny, ~98.5% reported
# accuracy) and export it to ONNX with preprocessing normalization baked in.
#
# Output: ml/nsfw_marqo_384.onnx
#   input : pixel_values  float32 [batch, 3, 384, 384], RGB scaled to [0, 1]
#           (resize shortest-side-to-384 + center-crop before scaling)
#   output: nsfw_score    float32 [batch, 1], P(NSFW) in [0, 1]
#
# The output contract matches ml/nsfw_classifier.onnx (the LAION head), so
# the Rust side only differs in what it feeds the model (pixels vs CLIP
# embedding); NsfwClassifier auto-detects which kind a file is by its input
# rank.
#
# Usage (requires network access to huggingface.co):
#   python scripts/setup_marqo_nsfw.py
import json
from pathlib import Path

import timm
import torch

OUT = Path("ml/nsfw_marqo_384.onnx")


class Wrapped(torch.nn.Module):
    """Bake normalization + softmax into the graph so the Rust caller only
    resizes/crops and scales to [0, 1]."""

    def __init__(self, model, mean, std, nsfw_index):
        super().__init__()
        self.model = model
        self.register_buffer("mean", torch.tensor(mean).view(1, 3, 1, 1))
        self.register_buffer("std", torch.tensor(std).view(1, 3, 1, 1))
        self.nsfw_index = nsfw_index

    def forward(self, x):
        logits = self.model((x - self.mean) / self.std)
        probs = torch.softmax(logits, dim=1)
        return probs[:, self.nsfw_index : self.nsfw_index + 1]


def main():
    if OUT.exists():
        print(f"{OUT} already exists; delete it to re-export.")
        return

    model = timm.create_model("hf_hub:Marqo/nsfw-image-detection-384", pretrained=True)
    model.eval()

    cfg = timm.data.resolve_data_config({}, model=model)
    print("timm data config:", json.dumps({k: str(v) for k, v in cfg.items()}, indent=2))
    size = cfg["input_size"][-1]
    assert size == 384, f"expected 384 input, got {size}"

    label_names = getattr(model, "pretrained_cfg", {}).get("label_names")
    print("label names:", label_names)
    if not label_names:
        raise SystemExit("model config lacks label_names; refusing to guess NSFW index")
    nsfw_index = [n.lower() for n in label_names].index("nsfw")
    print("nsfw class index:", nsfw_index)

    wrapped = Wrapped(model, cfg["mean"], cfg["std"], nsfw_index)
    wrapped.eval()

    dummy = torch.rand(1, 3, size, size)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    torch.onnx.export(
        wrapped,
        dummy,
        str(OUT),
        input_names=["pixel_values"],
        output_names=["nsfw_score"],
        dynamic_axes={"pixel_values": {0: "batch"}, "nsfw_score": {0: "batch"}},
        opset_version=17,
    )
    # Newer torch exporters externalize weights into a sidecar .data file;
    # repack into one self-contained .onnx so the app config can point at a
    # single path that survives being copied around.
    import onnx

    m = onnx.load(str(OUT))
    onnx.save(m, str(OUT), save_as_external_data=False)
    sidecar = OUT.with_suffix(".onnx.data")
    if sidecar.exists():
        sidecar.unlink()
    print(f"exported {OUT} ({OUT.stat().st_size / 1e6:.1f} MB)")

    # Smoke test: random noise should score low, and outputs must be [0, 1].
    import numpy as np
    import onnxruntime as ort

    sess = ort.InferenceSession(str(OUT), providers=["CPUExecutionProvider"])
    rng = np.random.default_rng(0)
    batch = rng.random((2, 3, size, size), dtype=np.float32)
    scores = np.asarray(sess.run(None, {"pixel_values": batch})[0])
    print("noise scores:", scores.ravel())
    assert bool(((scores >= 0.0) & (scores <= 1.0)).all())
    print("OK")


if __name__ == "__main__":
    main()
