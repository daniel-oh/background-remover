#!/usr/bin/env python3
"""Export BiRefNet_lite to ONNX at a chosen size, as birefnet-lite-768.onnx was.

    git clone https://github.com/ZhengPeng7/BiRefNet upstream   # at f707963
    python -m venv venv && venv/bin/pip install torch torchvision timm kornia \\
        einops safetensors onnx onnxscript opencv-python-headless scipy
    curl -LO https://huggingface.co/ZhengPeng7/BiRefNet_lite/resolve/main/model.safetensors
    venv/bin/python scripts/export-birefnet.py upstream model.safetensors 768 out.onnx

The upstream exports are fixed at 1024. A smaller square keeps most of the
quality for much less time and memory: at 768 the Swin-T model needs about
1.2 GB at its peak against 3.2 GB at 1024, on the same photos (README).

The file it writes is not byte-for-byte the published one (the exporter
names some graph nodes differently each run), but it holds the same weights
and the same operations and gives identical outputs; checked against
birefnet-lite-768.onnx with PyTorch 2.15.

Uses PyTorch's dynamo exporter (opset 19, with ONNX's own DeformConv), not
the TorchScript one and its third-party DeformConv patch.
"""
import re
import sys

upstream, weights, side, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]

# upstream's config.py picks the backbone by index; the lite model is Swin-T.
config_path = f"{upstream}/config.py"
config = open(config_path).read()
lite = re.sub(
    r"(\n\s*'dino_v3_b', 'dino_v3_s_plus', 'dino_v3_s',\n\s*\])\[\d+\]",
    r"\1[6]",
    config,
)
lite = lite.replace("[os.path.expanduser('~'), '/workspace'][1]", "[os.path.expanduser('~'), '/workspace'][0]")
lite = lite.replace("self.compile = True", "self.compile = False")
if "][6]" not in lite:
    sys.exit("could not select the swin_v1_t backbone in upstream config.py")
open(config_path, "w").write(lite)
sys.path.insert(0, upstream)

import torch  # noqa: E402
from safetensors.torch import load_file  # noqa: E402
from models.birefnet import BiRefNet  # noqa: E402

net = BiRefNet(bb_pretrained=False)
missing, unexpected = net.load_state_dict(load_file(weights), strict=False)
if missing or unexpected:
    sys.exit(f"weights do not fit: {len(missing)} missing, {len(unexpected)} unexpected")
net.eval()


class LastMap(torch.nn.Module):
    """The model returns one map per decoder stage; the last is the mask."""

    def __init__(self, net):
        super().__init__()
        self.net = net

    def forward(self, x):
        return self.net(x)[-1]


with torch.no_grad():
    torch.onnx.export(
        LastMap(net),
        (torch.randn(1, 3, side, side),),
        dynamo=True,
        input_names=["input_image"],
        output_names=["output_image"],
        opset_version=19,
    ).save(out)
print(f"wrote {out}")
