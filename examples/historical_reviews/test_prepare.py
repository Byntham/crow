"""Check target-branch guidance using local Git history and fake GitHub metadata."""
import contextlib
import io
import json
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


class PreparationTest(unittest.TestCase):
    def test_target_guidance_survives_divergence_and_reverse_comparison(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = root / 'source'
            repo.mkdir()

            def git(*args):
                return subprocess.check_output([
                    'git', '-C', str(repo), '-c', 'user.name=Fixture',
                    '-c', 'user.email=fixture@example.invalid', *args,
                ], text=True, stderr=subprocess.DEVNULL).strip()

            def commit(guidance):
                (repo / 'AGENTS.md').write_text(guidance)
                git('add', '.')
                git('commit', '-m', guidance)
                return git('rev-parse', 'HEAD')

            git('init', '-b', 'main')
            ancestor = commit('Old target guidance')
            git('checkout', '-b', 'feature')
            head = commit('PR-supplied guidance')
            git('checkout', 'main')
            target = commit('Current target guidance')

            driver = root / 'driver'
            driver.mkdir()
            script = driver / 'prepare.py'
            shutil.copyfile(Path(__file__).with_name('prepare.py'), script)
            cases = [dict(id=name, repo='fixture/project', number=1,
                          baseTip=target, head=head, reverse=reverse)
                     for name, reverse in [('forward', False), ('reverse', True)]]
            (driver / 'cases.json').write_text(json.dumps(cases))
            metadata = dict(base=dict(sha=target, ref='main'), head=dict(sha=head),
                            title='Update', body='', html_url='https://example.invalid/pr/1')
            check_output = subprocess.check_output

            def local_command(argv, **kwargs):
                if argv[0] == 'gh':
                    self.assertEqual(argv[:2], ['gh', 'api'])
                    response = [{'filename': 'AGENTS.md'}] if '/files?' in argv[2] else metadata
                    return json.dumps(response)
                if argv[0] == 'git' and argv[3:6] == ['remote', 'add', 'origin']:
                    argv = [*argv[:-1], str(repo)]
                return check_output(argv, **kwargs)

            output = root / 'prepared'
            with patch.object(sys, 'argv', [str(script), str(output)]), \
                    patch('subprocess.check_output', side_effect=local_command), \
                    contextlib.redirect_stdout(io.StringIO()):
                runpy.run_path(str(script), run_name='__main__')

            for name, expected in [('forward', (ancestor, head)), ('reverse', (head, ancestor))]:
                with self.subTest(comparison=name):
                    source = json.loads((output / name / 'source.json').read_text())
                    self.assertEqual((source['base'], source['head']), expected)
                    self.assertEqual(source['targetSha'], target)
                    guidance = check_output([
                        'git', '-C', source['dir'], 'show', f"{source['targetSha']}:AGENTS.md",
                    ], text=True)
                    self.assertEqual(guidance, 'Current target guidance')


if __name__ == '__main__':
    unittest.main()
