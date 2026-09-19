"""Exercise replay cancellation and failure export without model or container calls."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest


HERE = Path(__file__).resolve().parent
CASES = json.loads((HERE / 'cases.json').read_text())[:3]


class ReplayTest(unittest.TestCase):
    def prepare(self, root, body):
        reviewer = root / 'reviewer.py'
        reviewer.write_text(f'#!{sys.executable}\n' + body)
        reviewer.chmod(0o700)
        for case in CASES:
            directory = root / case['id']
            directory.mkdir()
            (directory / 'source.json').write_text(json.dumps({'base': 'a' * 40, 'head': 'b' * 40}))
        return reviewer

    def command(self, root, reviewer, jobs=2):
        return [sys.executable, str(HERE / 'run.py'), str(root), '--reviewer', str(reviewer),
                '--podman', '/unused-podman', '--jobs', str(jobs),
                *[argument for case in CASES for argument in ['--case', case['id']]]]

    def wait_for(self, predicate):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(0.02)
        self.fail('Timed out waiting for the replay fixture')

    def test_signals_wait_for_active_reviewers_and_skip_queued_cases(self):
        for signum in [signal.SIGTERM, signal.SIGINT]:
            with self.subTest(signal=signum), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                reviewer = self.prepare(root, '''import json, os, signal, sys, time
from pathlib import Path
state = Path(sys.argv[2])
stopped = None
def interrupt(signum, frame):
    global stopped
    stopped = signum
signal.signal(signal.SIGTERM, interrupt)
signal.signal(signal.SIGINT, interrupt)
(state / 'ready.json').write_text(json.dumps({'pid': os.getpid()}))
while stopped is None:
    time.sleep(0.01)
(state / 'cleanup-started').write_text(str(stopped))
while not (state / 'allow-cleanup').exists():
    time.sleep(0.01)
(state / 'cleaned').write_text('reviewer cleanup completed')
sys.exit(23)
''')
                states = [root / case['id'] / 'baseline' for case in CASES[:2]]
                driver = subprocess.Popen(self.command(root, reviewer), stdout=subprocess.PIPE,
                                          stderr=subprocess.PIPE, text=True)
                children = []
                try:
                    self.wait_for(lambda: all((state / 'ready.json').exists() for state in states))
                    children = [json.loads((state / 'ready.json').read_text())['pid'] for state in states]
                    driver.send_signal(signum)
                    self.wait_for(lambda: all((state / 'cleanup-started').exists() for state in states))
                    self.assertIsNone(driver.poll(), 'Driver exited before reviewer cleanup')
                    for child in children:
                        os.kill(child, 0)
                    for state in states:
                        self.assertEqual(int((state / 'cleanup-started').read_text()), signum)
                        (state / 'allow-cleanup').touch()
                    stdout, stderr = driver.communicate(timeout=10)
                    self.assertEqual(driver.returncode, 128 + signum, (stdout, stderr))
                    for state in states:
                        self.assertTrue((state / 'cleaned').exists())
                        receipt = json.loads((state / 'replay.json').read_text())
                        self.assertTrue(receipt['interrupted'])
                        self.assertEqual(receipt['exitCode'], 23)
                        self.assertEqual(receipt['signal'], signal.Signals(signum).name)
                    self.assertFalse((root / CASES[2]['id'] / 'baseline').exists())
                    for child in children:
                        with self.assertRaises(ProcessLookupError):
                            os.kill(child, 0)
                finally:
                    if driver.poll() is None:
                        driver.kill()
                        driver.communicate(timeout=10)
                    for state in states:
                        if (state / 'ready.json').exists():
                            children.append(json.loads((state / 'ready.json').read_text())['pid'])
                    for child in set(children):
                        try:
                            os.kill(child, signal.SIGKILL)
                        except ProcessLookupError:
                            pass

    def test_export_includes_failures_before_provider_events_exist(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            reviewer = self.prepare(root, '''import sys
from pathlib import Path
(Path(sys.argv[2]) / 'error.txt').write_text('Checkout failed before provider startup\\n')
sys.exit(7)
''')
            result = subprocess.run(self.command(root, reviewer, jobs=1), capture_output=True,
                                    text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            case = root / CASES[0]['id']
            for attempt, filename, body in [
                ('error-only', 'error.txt', 'Early configuration failure'),
                ('report-only', 'result.json', json.dumps({'summary': 'Completed report'})),
                ('receipt-only', 'replay.json', json.dumps({'exitCode': 9})),
            ]:
                state = case / attempt
                state.mkdir()
                (state / filename).write_text(body)
            runtime = case / 'runtime-only' / 'reviews' / CASES[0]['id'] / 'experiments'
            runtime.mkdir(parents=True)
            (runtime / 'one.json').write_text(json.dumps({'status': 'blocked'}))
            (case / 'unrelated-directory').mkdir()
            output = root / 'export.json'
            exported = subprocess.run([sys.executable, str(HERE / 'export.py'), str(root), str(output)],
                                      capture_output=True, text=True, timeout=10)
            self.assertEqual(exported.returncode, 0, exported.stderr)
            attempts = {attempt['name']: attempt for attempt in json.loads(output.read_text())[0]['attempts']}
            self.assertEqual(set(attempts), {'baseline', 'error-only', 'report-only', 'receipt-only', 'runtime-only'})
            self.assertEqual(attempts['baseline']['replay']['exitCode'], 7)
            self.assertFalse(attempts['baseline']['replay']['interrupted'])
            self.assertIn('Checkout failed before provider startup', attempts['baseline']['error'])
            self.assertEqual(attempts['runtime-only']['receipts'], [{'status': 'blocked'}])
            self.assertEqual(attempts['report-only']['report'], {'summary': 'Completed report'})
            for attempt in attempts.values():
                self.assertEqual(attempt['toolCalls'], {})
                self.assertEqual(attempt['viewedArtifacts'], [])


if __name__ == '__main__':
    unittest.main()
