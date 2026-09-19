"""Create a Python regression and pre-existing failure with no Crow instructions."""
import argparse
import json
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('output', type=Path)
root = parser.parse_args().output.resolve()
root.mkdir(parents=True, exist_ok=False)
repo = root / 'repository'
repo.mkdir()
def git(*args):
    return subprocess.check_output(['git', '-C', str(repo), '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', *args], text=True).strip()
git('init', '-b', 'main')
(repo / 'requirements.txt').write_text('packaging==24.2\n')
(repo / 'README.md').write_text('# Release helpers\n\nInstall with `pip install -r requirements.txt`. Run checks with `python -m pytest`.\n')
(repo / 'releases.py').write_text('from packaging.version import Version\n\ndef latest(versions):\n    return max(versions, key=Version)\n\ndef legacy_total():\n    return 1\n')
(repo / 'test_releases.py').write_text('from releases import latest, legacy_total\n\ndef test_latest():\n    assert latest(["1.0", "2.0"]) == "2.0"\n\ndef test_legacy_total():\n    assert legacy_total() == 2\n')
git('add', '.')
git('commit', '-m', 'Release helpers')
base = git('rev-parse', 'HEAD')
(repo / 'releases.py').write_text('from packaging.version import Version\n\ndef latest(versions):\n    return max(versions)\n\ndef legacy_total():\n    return 1\n')
git('commit', '-am', 'Simplify version selection')
head = git('rev-parse', 'HEAD')
source = {'dir': str(repo), 'base': base, 'head': head, 'target': 'main', 'targetSha': base, 'prContext': {'title': 'Simplify version selection', 'body': 'Simplify the release helper implementation.'}}
(root / 'source.json').write_text(json.dumps(source, indent=2))
print(json.dumps(source, indent=2))
