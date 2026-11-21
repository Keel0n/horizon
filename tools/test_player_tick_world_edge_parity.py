#!/usr/bin/env python3
"""Compare vanilla `Player.tick`'s world edge with Horizon's remote player tick.

The Java oracle runs the shipped `Player.tick` once from each start position, on a
spectator that flies and does not move, and prints where the player ends. Horizon runs
the same start positions through the tick a client of a server ticks its own player
with, its column lit as a server's arrival lights it, and prints the same words. Only
the position after the tick is compared: the move inside the tick (`travel`/`aiStep` are
the fixture), the seat a passenger is put on (`Entity.rideTick`, its own test) and the
level of detail are outside it.
"""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile

try:
    from . import lab, oracle_harness as harness, test_rust_parity as base
    from . import test_model_baking_parity as baking
except ImportError:
    import lab
    import oracle_harness as harness
    import test_rust_parity as base
    import test_model_baking_parity as baking

ORACLE = 'VanillaPlayerTickOracle'
TEST = ('game::remote::movement_input_tests::player_world_edge_parity_tests::'
        'a_tick_from_the_world_edge_lands_like_vanilla')
CASES = ['inside_x', 'past_x', 'past_negative_x', 'past_z', 'past_negative_z', 'on_the_edge',
         'at_the_clamp', 'high_y', 'past_negative_x_and_z']


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--resources', type=Path, default=lab.RESOURCES)
    args = parser.parse_args()
    lock, jars, _ = base.verify_artifacts()
    build = lab.LOCAL / 'player-tick-world-edge-parity'
    build.mkdir(parents=True, exist_ok=True)
    report = build / 'report.json'
    report.unlink(missing_ok=True)
    run = build / 'rust.json'
    run.unlink(missing_ok=True)
    sources = [lab.ROOT / path for path in (
        'tools/test_player_tick_world_edge_parity.py',
        'tools/oracle_harness.py',
        'tools/test_rust_parity.py',
        'tools/lab.py',
        'crates/horizon-client/src/game/network/remote/tick.rs',
        'crates/horizon-client/src/game/network/remote/movement_input_tests.rs',
        'crates/horizon-client/src/game/network/remote/player_world_edge_parity_tests.rs',
        f'mods/horizon/src/test/java/dev/horizonlab/parity/{ORACLE}.java')]
    hashes = base.source_hashes(sources)
    classes, dependencies, _ = harness.compile_oracle(
        build, ORACLE, jars, baking.client_libraries(), lock['minecraft']['java_major'])
    harness.java_run(build, classes, dependencies, ORACLE, [build / 'java.json'], 'java')
    # The probe's trace goes to a scratch folder: it is one more output of the run, not a
    # build artifact, and a test binary's own writes are the run's, not the driver's.
    with tempfile.TemporaryDirectory(prefix='player-tick-world-edge-') as scratch:
        trace = Path(scratch) / 'rust.json'
        env = lab.game_env(HORIZON_TEST_RESOURCES=args.resources.resolve(),
                           HORIZON_PLAYER_TICK_TRACE=trace)
        with (build / 'rust.log').open('w') as log:
            subprocess.run(
                ['cargo', 'test', '--locked', '--profile', 'fast', '-p', 'horizon-client', '--lib',
                 TEST, '--', '--ignored', '--exact'],
                cwd=lab.ROOT, env=env, stdout=log, stderr=subprocess.STDOUT,
                check=True, timeout=3600)
        if not trace.is_file():
            raise AssertionError(f'the Rust probe wrote no trace: {build / "rust.log"}')
        run.write_bytes(trace.read_bytes())
    vanilla = json.loads((build / 'java.json').read_text())
    rust = json.loads(run.read_text())
    covered = [row['case'] for row in vanilla]
    if covered != CASES:
        raise AssertionError(f'vanilla oracle covers {covered}, not {CASES}')
    if rust != vanilla:
        raise AssertionError(f'the world edge differs: vanilla {vanilla}; Horizon {rust}')
    base.ensure_sources_unchanged(sources, hashes)
    lab.write_json(report, dict(
        schema=1, minecraft=lock['minecraft']['version'], cases=len(vanilla),
        sources_sha256=hashes, client_jar_sha256=lab.digest(jars['client'], 'sha256'),
        java_sha256=lab.digest(build / 'java.json', 'sha256'),
        rust_sha256=lab.digest(run, 'sha256'), scope=__doc__))
    print(f'{len(vanilla)} world-edge player ticks match vanilla: {report}')


if __name__ == '__main__':
    main()