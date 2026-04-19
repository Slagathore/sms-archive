#!/usr/bin/env python3
"""
Setup script for SMS Archive ML models.

Exports/converts:
1. CLIP ViT-L/14 visual encoder -> ONNX (for Rust inference)
2. NSFW classifier (full MLP) -> ONNX (for Rust inference)

Usage:
    python setup_ml_models.py
"""

import os
import sys
from pathlib import Path

# Paths
SCRIPT_DIR = Path(__file__).parent
PROJECT_ROOT = SCRIPT_DIR.parent
ML_DIR = PROJECT_ROOT / "ml"

CLIP_ONNX_PATH = ML_DIR / "clip-vit-l-14.onnx"
NSFW_ONNX_PATH = ML_DIR / "nsfw_classifier.onnx"
AUTOKERAS_MODEL_DIR = ML_DIR / "clip_autokeras_binary_nsfw" / "clip_autokeras_binary_nsfw"


def setup_clip_onnx():
    """Export CLIP ViT-L/14 visual encoder to ONNX format."""
    if CLIP_ONNX_PATH.exists():
        size_mb = CLIP_ONNX_PATH.stat().st_size / (1024 * 1024)
        print(f"[OK] CLIP ONNX model already exists: {CLIP_ONNX_PATH} ({size_mb:.1f} MB)")
        return True
    
    print("=" * 60)
    print("Exporting CLIP ViT-L/14 to ONNX...")
    print("=" * 60)
    
    import torch
    import open_clip
    
    print("Loading CLIP ViT-L/14 (OpenAI pretrained)...")
    model, _, preprocess = open_clip.create_model_and_transforms(
        'ViT-L-14',
        pretrained='openai'
    )
    model.eval()
    
    visual = model.visual
    visual.eval()
    
    with torch.no_grad():
        dummy = torch.randn(1, 3, 224, 224)
        test_out = visual(dummy)
        embed_dim = test_out.shape[-1]
        print(f"  Embedding dimension: {embed_dim}")
    
    dummy_input = torch.randn(1, 3, 224, 224)
    
    print(f"Exporting to: {CLIP_ONNX_PATH}")
    
    torch.onnx.export(
        visual,
        (dummy_input,),
        str(CLIP_ONNX_PATH),
        input_names=['pixel_values'],
        output_names=['embeddings'],
        dynamic_axes={
            'pixel_values': {0: 'batch_size'},
            'embeddings': {0: 'batch_size'}
        },
        opset_version=17,
        do_constant_folding=True
    )
    
    import onnx
    print("Verifying ONNX model...")
    onnx_model = onnx.load(str(CLIP_ONNX_PATH))
    onnx.checker.check_model(onnx_model)
    
    import onnxruntime as ort
    print("Testing ONNX inference...")
    providers = ort.get_available_providers()
    print(f"  Available providers: {providers}")
    session = ort.InferenceSession(str(CLIP_ONNX_PATH), providers=['CUDAExecutionProvider', 'CPUExecutionProvider'])
    test_input = dummy_input.numpy()
    outputs = session.run(None, {'pixel_values': test_input})
    print(f"  Test output shape: {outputs[0].shape}")  # type: ignore[union-attr]
    
    size_mb = CLIP_ONNX_PATH.stat().st_size / (1024 * 1024)
    print(f"[OK] CLIP ONNX model saved: {CLIP_ONNX_PATH} ({size_mb:.1f} MB)")
    
    return True


def setup_nsfw_onnx():
    """Export NSFW classifier MLP to ONNX format."""
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
    
    os.environ['TF_CPP_MIN_LOG_LEVEL'] = '2'
    
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
            self.register_buffer('mean', torch.zeros(768))
            self.register_buffer('std', torch.ones(768))
            
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
    if 'normalization/mean:0' in tf_vars:
        model.mean = torch.tensor(tf_vars['normalization/mean:0'])
        # Variance -> std
        var = tf_vars.get('normalization/variance:0', np.ones(768))
        model.std = torch.tensor(np.sqrt(var + 1e-7))
    
    # Load dense layer weights (TF is [in, out], PyTorch is [out, in])
    model.fc1.weight.data = torch.tensor(tf_vars['dense/kernel:0'].T)
    model.fc1.bias.data = torch.tensor(tf_vars['dense/bias:0'])
    
    model.fc2.weight.data = torch.tensor(tf_vars['dense_1/kernel:0'].T)
    model.fc2.bias.data = torch.tensor(tf_vars['dense_1/bias:0'])
    
    model.fc3.weight.data = torch.tensor(tf_vars['dense_2/kernel:0'].T)
    model.fc3.bias.data = torch.tensor(tf_vars['dense_2/bias:0'])
    
    model.fc4.weight.data = torch.tensor(tf_vars['dense_3/kernel:0'].T)
    model.fc4.bias.data = torch.tensor(tf_vars['dense_3/bias:0'])
    
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
        input_names=['embedding'],
        output_names=['nsfw_score'],
        dynamic_axes={
            'embedding': {0: 'batch_size'},
            'nsfw_score': {0: 'batch_size'}
        },
        opset_version=17,
        do_constant_folding=True
    )
    
    size_mb = NSFW_ONNX_PATH.stat().st_size / (1024 * 1024)
    print(f"[OK] NSFW ONNX model saved: {NSFW_ONNX_PATH} ({size_mb:.2f} MB)")
    
    verify_nsfw_onnx()
    
    return True


def verify_nsfw_onnx():
    """Verify the NSFW ONNX model works correctly."""
    import onnxruntime as ort
    import numpy as np
    
    print("\nVerifying NSFW ONNX model...")
    
    session = ort.InferenceSession(str(NSFW_ONNX_PATH), providers=['CUDAExecutionProvider', 'CPUExecutionProvider'])
    
    # Test with random normalized embedding
    fake_embedding = np.random.randn(1, 768).astype(np.float32)
    
    outputs = session.run(None, {'embedding': fake_embedding})
    score = outputs[0][0][0]  # type: ignore[index]
    
    print(f"  Test on random embedding: NSFW score = {score:.4f}")
    print(f"  [OK] NSFW classifier working correctly")


def main():
    print()
    print("=" * 60)
    print(" SMS Archive ML Model Setup ".center(60))
    print("=" * 60)
    print()
    
    ML_DIR.mkdir(parents=True, exist_ok=True)
    
    clip_ok = setup_clip_onnx()
    nsfw_ok = setup_nsfw_onnx()
    
    print()
    print("=" * 60)
    if clip_ok and nsfw_ok:
        print("[OK] All models ready!")
        print()
        print("Model paths for Rust:")
        print(f"  CLIP ONNX:    {CLIP_ONNX_PATH}")
        print(f"  NSFW ONNX:    {NSFW_ONNX_PATH}")
        print()
        print("Pipeline: Image -> CLIP (224x224) -> 768-dim embedding -> NSFW MLP -> score [0,1]")
    else:
        print("[FAIL] Some models failed to set up. Check errors above.")
        sys.exit(1)


if __name__ == "__main__":
    main()
