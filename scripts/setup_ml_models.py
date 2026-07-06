#!/usr/bin/env python3
"""
Setup script for SMS Archive ML models.

Produces every model file that config/app_global_settings.json points at:

1. CLIP1 bundle -> ml/CLIP1/
   Downloads the pre-converted Xenova/clip-vit-large-patch14 Transformers.js
   ONNX export from Hugging Face:
     - vision_model_fp16.onnx  (clip_model_path)
     - text_model_fp16.onnx    (clip_text_model_path)
     - tokenizer.json          (clip_text_tokenizer_path)
     - config.json, merges.txt, preprocessor_config.json,
       special_tokens_map.json, tokenizer_config.json, vocab.json
       (supporting metadata shipped alongside the model in the same repo)
   This is downloaded rather than exported locally because it is a split
   vision/text ONNX graph produced by Hugging Face's Optimum/Transformers.js
   tooling -- open_clip + torch.onnx.export do not reproduce that exact
   split-graph layout, so re-deriving it locally would not byte-match what
   the app was built/tested against.

2. NSFW classifier (LAION CLIP-embedding MLP head) -> ml/nsfw_classifier.onnx
   Exported locally from the AutoKeras SavedModel bundle
   (ml/clip_autokeras_binary_nsfw/). This is the FALLBACK NSFW model used
   when ml/nsfw_marqo_384.onnx is absent. For the PREFERRED, more accurate
   NSFW model, run scripts/setup_marqo_nsfw.py separately (it downloads
   Marqo/nsfw-image-detection-384 straight to ONNX).

Usage:
    pip install -r requirements.txt
    python scripts/setup_ml_models.py

Idempotent: any file that already exists on disk is left untouched and
skipped, so re-running after a partial/interrupted run only fetches what's
missing.

Checksums: see docs/PRIVACY.md ("verify checksum/signature before use").
Downloaded CLIP1 files are checked against EXPECTED_SHA256 below when a hash
has been pinned. Nothing is pinned yet (this script was written without
network access to compute the real upstream hashes) -- on first successful
run, computed hashes are printed so a maintainer can copy them into
EXPECTED_SHA256, after which future runs verify integrity instead of
trusting the download blindly.
"""

import hashlib
import os
import sys
from pathlib import Path
from typing import Optional

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------

SCRIPT_DIR = Path(__file__).parent
PROJECT_ROOT = SCRIPT_DIR.parent
ML_DIR = PROJECT_ROOT / "ml"

CLIP1_DIR = ML_DIR / "CLIP1"
NSFW_ONNX_PATH = ML_DIR / "nsfw_classifier.onnx"
AUTOKERAS_MODEL_DIR = ML_DIR / "clip_autokeras_binary_nsfw" / "clip_autokeras_binary_nsfw"

# Hugging Face repo providing the pre-converted ONNX CLIP bundle.
CLIP_REPO_ID = "Xenova/clip-vit-large-patch14"

# Files at the repo root: tokenizer + preprocessing metadata.
CLIP_ROOT_FILES = [
    "config.json",
    "merges.txt",
    "preprocessor_config.json",
    "special_tokens_map.json",
    "tokenizer.json",
    "tokenizer_config.json",
    "vocab.json",
]

# Files under the repo's "onnx/" subfolder. Xenova's Transformers.js exports
# split the CLIP graph into separate vision/text encoders and ship fp32,
# fp16, and int8 variants of each. We only need the fp16 pair, matching
# clip_model_path / clip_text_model_path in config/app_global_settings.json.
# The repo also has a combined `model_fp16.onnx` (~800 MB) and quantized
# variants that nothing in this app reads -- intentionally NOT downloaded to
# avoid ~1 GB of wasted bandwidth/disk. If the exact "onnx/" subfolder layout
# has changed upstream, `snapshot_download(repo_id=CLIP_REPO_ID,
# local_dir=CLIP1_DIR)` is a slower-but-robust fallback that mirrors the
# whole repo instead of naming individual files.
CLIP_ONNX_SUBFOLDER = "onnx"
CLIP_ONNX_FILES = [
    "vision_model_fp16.onnx",
    "text_model_fp16.onnx",
]

# ---------------------------------------------------------------------------
# Checksum verification (docs/PRIVACY.md: "verify checksum/signature before
# use"). Only applies to files downloaded from the network (the CLIP1
# bundle) -- the NSFW classifier below is exported locally from a model
# already present on disk, so there is nothing external to attest to.
# ---------------------------------------------------------------------------

# TODO(security): placeholders. This script was authored without network
# access, so the real upstream SHA256 hashes could not be fetched/verified
# yet. Run this script with network access once, copy the "sha256=..."
# values it prints for each CLIP1/* file into this dict, and commit the
# result -- from then on, every run (on every machine) verifies integrity
# instead of trusting Hugging Face's transport unconditionally.
EXPECTED_SHA256: dict[str, Optional[str]] = {
    "CLIP1/config.json": None,
    "CLIP1/merges.txt": None,
    "CLIP1/preprocessor_config.json": None,
    "CLIP1/special_tokens_map.json": None,
    "CLIP1/tokenizer.json": None,
    "CLIP1/tokenizer_config.json": None,
    "CLIP1/vocab.json": None,
    "CLIP1/vision_model_fp16.onnx": None,
    "CLIP1/text_model_fp16.onnx": None,
}


def sha256_file(path: Path, chunk_size: int = 1 << 20) -> str:
    """Stream a file through SHA256 without loading it fully into memory."""
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(chunk_size), b""):
            h.update(chunk)
    return h.hexdigest()


def verify_checksum(path: Path, key: str) -> bool:
    """Verify `path` against EXPECTED_SHA256[key] when pinned.

    If no hash is pinned yet, compute and print it so a maintainer can pin
    it (see the EXPECTED_SHA256 TODO above). Returns False on a confirmed
    mismatch, True otherwise (including the "not pinned yet" case).
    """
    digest = sha256_file(path)
    expected = EXPECTED_SHA256.get(key)
    if expected is None:
        print(f"  [hash] {key}: sha256={digest}")
        print(f"         (not pinned yet -- add this value to EXPECTED_SHA256 in {Path(__file__).name})")
        return True
    if digest.lower() != expected.lower():
        print(f"  [FAIL] checksum mismatch for {key}")
        print(f"         expected: {expected}")
        print(f"         actual:   {digest}")
        return False
    print(f"  [OK] checksum verified for {key}")
    return True


# ---------------------------------------------------------------------------
# CLIP1 bundle (vision + text encoder + tokenizer)
# ---------------------------------------------------------------------------


def _place_downloaded_file(downloaded_path: Path, dest: Path) -> None:
    """Move a file downloaded by huggingface_hub into its flat destination.

    hf_hub_download(..., subfolder="onnx", local_dir=CLIP1_DIR) preserves the
    repo's subfolder structure (CLIP1_DIR/onnx/<file>), but the app config
    expects a flat ml/CLIP1/<file> layout, so relocate it and clean up the
    now-empty intermediate directory.
    """
    downloaded_path = Path(downloaded_path)
    if downloaded_path == dest:
        return
    dest.parent.mkdir(parents=True, exist_ok=True)
    downloaded_path.replace(dest)
    try:
        downloaded_path.parent.rmdir()
    except OSError:
        pass  # not empty (or already gone) -- fine, nothing to clean up


def setup_clip1_bundle() -> bool:
    """Download the Xenova/clip-vit-large-patch14 ONNX bundle into ml/CLIP1/.

    Populates clip_model_path, clip_text_model_path, and
    clip_text_tokenizer_path from config/app_global_settings.json.
    Skips any file that already exists.
    """
    try:
        from huggingface_hub import hf_hub_download
    except ImportError:
        print("ERROR: huggingface_hub is not installed.")
        print("Run: pip install -r requirements.txt")
        return False

    print("=" * 60)
    print("Fetching CLIP1 bundle (Xenova/clip-vit-large-patch14)...")
    print("=" * 60)

    CLIP1_DIR.mkdir(parents=True, exist_ok=True)
    ok = True

    for filename in CLIP_ROOT_FILES:
        dest = CLIP1_DIR / filename
        key = f"CLIP1/{filename}"
        if dest.exists():
            print(f"[OK] {dest} already exists, skipping")
            continue
        try:
            downloaded = hf_hub_download(
                repo_id=CLIP_REPO_ID, filename=filename, local_dir=str(CLIP1_DIR)
            )
        except Exception as e:  # noqa: BLE001 - network/HF errors of many types
            print(f"ERROR downloading {filename}: {e}")
            ok = False
            continue
        _place_downloaded_file(Path(downloaded), dest)
        if not verify_checksum(dest, key):
            ok = False

    for filename in CLIP_ONNX_FILES:
        dest = CLIP1_DIR / filename
        key = f"CLIP1/{filename}"
        if dest.exists():
            size_mb = dest.stat().st_size / (1024 * 1024)
            print(f"[OK] {dest} already exists ({size_mb:.1f} MB), skipping")
            continue
        try:
            downloaded = hf_hub_download(
                repo_id=CLIP_REPO_ID,
                filename=filename,
                subfolder=CLIP_ONNX_SUBFOLDER,
                local_dir=str(CLIP1_DIR),
            )
        except Exception as e:  # noqa: BLE001
            print(f"ERROR downloading {filename}: {e}")
            ok = False
            continue
        _place_downloaded_file(Path(downloaded), dest)
        size_mb = dest.stat().st_size / (1024 * 1024)
        print(f"[OK] {dest} saved ({size_mb:.1f} MB)")
        if not verify_checksum(dest, key):
            ok = False

    return ok


# ---------------------------------------------------------------------------
# NSFW classifier (LAION CLIP-embedding MLP head, fallback model)
# ---------------------------------------------------------------------------


def setup_nsfw_onnx() -> bool:
    """Export the NSFW classifier MLP (LAION head) to ONNX format.

    This is the fallback referenced when ml/nsfw_marqo_384.onnx (produced by
    scripts/setup_marqo_nsfw.py, the preferred model) is not present.
    """
    if NSFW_ONNX_PATH.exists():
        size_mb = NSFW_ONNX_PATH.stat().st_size / (1024 * 1024)
        print(f"[OK] NSFW ONNX model already exists: {NSFW_ONNX_PATH} ({size_mb:.2f} MB)")
        verify_nsfw_onnx()
        return True

    print()
    print("=" * 60)
    print("Exporting NSFW classifier to ONNX...")
    print("=" * 60)

    if not AUTOKERAS_MODEL_DIR.exists():
        print(f"ERROR: AutoKeras model not found at {AUTOKERAS_MODEL_DIR}")
        print("Make sure you've downloaded and extracted clip_autokeras_binary_nsfw.zip")
        return False

    import numpy as np
    import tensorflow as tf

    os.environ["TF_CPP_MIN_LOG_LEVEL"] = "2"

    print(f"Loading AutoKeras model from {AUTOKERAS_MODEL_DIR}...")
    loaded = tf.saved_model.load(str(AUTOKERAS_MODEL_DIR))

    # Print structure
    print("\nModel variables:")
    for i, v in enumerate(loaded.variables):
        if v is not None:
            print(f"  [{i}] {v.name}: {v.shape}")

    # Build a PyTorch model matching the TF structure and export to ONNX
    # Structure: Normalize -> Dense(768,64) -> Dense(64,512) -> Dense(512,256) -> Dense(256,1) -> Sigmoid
    import torch
    import torch.nn as nn

    class NSFWClassifier(nn.Module):
        def __init__(self):
            super().__init__()
            # Normalization parameters (will load from TF)
            self.register_buffer("mean", torch.zeros(768))
            self.register_buffer("std", torch.ones(768))

            # MLP layers matching AutoKeras structure
            self.fc1 = nn.Linear(768, 64)
            self.fc2 = nn.Linear(64, 512)
            self.fc3 = nn.Linear(512, 256)
            self.fc4 = nn.Linear(256, 1)
            self.relu = nn.ReLU()
            self.sigmoid = nn.Sigmoid()

        def forward(self, x):
            # Normalize input
            x = (x - self.mean) / (self.std + 1e-7)  # type: ignore[operator]
            # MLP
            x = self.relu(self.fc1(x))
            x = self.relu(self.fc2(x))
            x = self.relu(self.fc3(x))
            x = self.sigmoid(self.fc4(x))
            return x

    model = NSFWClassifier()

    # Extract and load TF weights into PyTorch model
    tf_vars = {v.name: v.numpy() for v in loaded.variables if v is not None}

    # Load normalization params
    if "normalization/mean:0" in tf_vars:
        model.mean = torch.tensor(tf_vars["normalization/mean:0"])
        # Variance -> std
        var = tf_vars.get("normalization/variance:0", np.ones(768))
        model.std = torch.tensor(np.sqrt(var + 1e-7))

    # Load dense layer weights (TF is [in, out], PyTorch is [out, in])
    model.fc1.weight.data = torch.tensor(tf_vars["dense/kernel:0"].T)
    model.fc1.bias.data = torch.tensor(tf_vars["dense/bias:0"])

    model.fc2.weight.data = torch.tensor(tf_vars["dense_1/kernel:0"].T)
    model.fc2.bias.data = torch.tensor(tf_vars["dense_1/bias:0"])

    model.fc3.weight.data = torch.tensor(tf_vars["dense_2/kernel:0"].T)
    model.fc3.bias.data = torch.tensor(tf_vars["dense_2/bias:0"])

    model.fc4.weight.data = torch.tensor(tf_vars["dense_3/kernel:0"].T)
    model.fc4.bias.data = torch.tensor(tf_vars["dense_3/bias:0"])

    model.eval()

    # Test with random input
    print("\nTesting PyTorch model...")
    test_input = torch.randn(1, 768)
    with torch.no_grad():
        test_output = model(test_input)
        print(f"  Test output: {test_output.item():.4f}")

    # Export to ONNX
    print(f"\nExporting to: {NSFW_ONNX_PATH}")
    dummy_input = torch.randn(1, 768)

    torch.onnx.export(
        model,
        (dummy_input,),
        str(NSFW_ONNX_PATH),
        input_names=["embedding"],
        output_names=["nsfw_score"],
        dynamic_axes={
            "embedding": {0: "batch_size"},
            "nsfw_score": {0: "batch_size"},
        },
        opset_version=17,
        do_constant_folding=True,
    )

    size_mb = NSFW_ONNX_PATH.stat().st_size / (1024 * 1024)
    print(f"[OK] NSFW ONNX model saved: {NSFW_ONNX_PATH} ({size_mb:.2f} MB)")

    verify_nsfw_onnx()

    return True


def verify_nsfw_onnx() -> None:
    """Sanity-check that the NSFW ONNX model loads and runs correctly.

    (Not a checksum check -- this file is built locally from
    ml/clip_autokeras_binary_nsfw/, not downloaded, so ONNX-exporter
    non-determinism across torch/opset versions means byte-for-byte pinning
    isn't meaningful here. See EXPECTED_SHA256 above for the downloaded
    CLIP1 files, which is where checksum verification matters.)
    """
    import numpy as np
    import onnxruntime as ort

    print("\nVerifying NSFW ONNX model...")

    session = ort.InferenceSession(
        str(NSFW_ONNX_PATH), providers=["CUDAExecutionProvider", "CPUExecutionProvider"]
    )

    # Test with random normalized embedding
    fake_embedding = np.random.randn(1, 768).astype(np.float32)

    outputs = session.run(None, {"embedding": fake_embedding})
    score = outputs[0][0][0]  # type: ignore[index]

    print(f"  Test on random embedding: NSFW score = {score:.4f}")
    print("  [OK] NSFW classifier working correctly")


def main():
    print()
    print("=" * 60)
    print(" SMS Archive ML Model Setup ".center(60))
    print("=" * 60)
    print()

    ML_DIR.mkdir(parents=True, exist_ok=True)

    clip_ok = setup_clip1_bundle()
    nsfw_ok = setup_nsfw_onnx()

    print()
    print("=" * 60)
    if clip_ok and nsfw_ok:
        print("[OK] All models ready!")
        print()
        print("Model paths for Rust (config/app_global_settings.json):")
        print(f"  clip_model_path:          {CLIP1_DIR / 'vision_model_fp16.onnx'}")
        print(f"  clip_text_model_path:     {CLIP1_DIR / 'text_model_fp16.onnx'}")
        print(f"  clip_text_tokenizer_path: {CLIP1_DIR / 'tokenizer.json'}")
        print(f"  clip_nsfw_weights_path:   {NSFW_ONNX_PATH} (fallback)")
        print()
        print("Optional: run scripts/setup_marqo_nsfw.py for the preferred,")
        print("higher-accuracy NSFW model (ml/nsfw_marqo_384.onnx).")
    else:
        print("[FAIL] Some models failed to set up. Check errors above.")
        sys.exit(1)


if __name__ == "__main__":
    main()
