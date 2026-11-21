# Evidence index

The package has no version-control history (it is delivered as a zip), so this file is the record of exactly which files the clean run covered. Each file is listed with its SHA-256 checksum. To check a copy, run from the repository root:

```sh
sha256sum -c docs/evidence/MANIFEST.sha256
```

## Evidence assets

| Asset | Location | What it shows |
|---|---|---|
| Clean-run log | [`evidence/clean-run-2026-11-11.log`](evidence/clean-run-2026-11-11.log) | every command and its output from the reproducibility pass; ends with `RESULT: CLEAN RUN PASSED` |
| Checksum manifest | [`evidence/MANIFEST.sha256`](evidence/MANIFEST.sha256) | SHA-256 of every file in the package at the time of the run |
| Golden output | [`../tests/fixtures/smoke.expected`](../tests/fixtures/smoke.expected) | the exact output the smoke script must produce |
| Smoke input | [`../tests/fixtures/smoke.commands`](../tests/fixtures/smoke.commands) | the scripted session |
| Hardening tests | [`../tests/hardening.rs`](../tests/hardening.rs) | 23 failure-path tests, 5 of them regression tests for DEFECT-01 |
| Audit record | [`AUDIT.md`](AUDIT.md) | what was found in the received package and what was changed |
| Reproducibility record | [`REPRODUCIBILITY.md`](REPRODUCIBILITY.md) | method, result, friction points, open items |

## Files changed and unchanged from the received package

Verified byte-for-byte against the uploaded `polydb.zip`: `Cargo.toml`, `Cargo.lock`, `tests/integration.rs`, `tests/drivers.py`, and nine of the thirteen files under `src/`. The other four (`value.rs`, `sql.rs`, `doc.rs`, `mongo.rs`) carry the DEFECT-01 fix: 145 lines added and 11 changed. `README.md` was rewritten; its command reference moved to `docs/REFERENCE.md`. Every other file is new.

## Manifest

```
6c2aba9d3df79e483dbfa50073ae4060c2e36d1abecc7906ea5bd833b681764f  ./.dockerignore
0a81df2eb622f40de69a234a8e68787f071897903fbed405d1c0dfd4ed82b028  ./.github/workflows/ci.yml
d7930e268f85e43076013bb5738177a33180cab799f8994e574c462f722adfe8  ./.gitignore
3d25429860edd59eded5f766c58988b1c07ebf46a4caf797602b87fa1cc859b2  ./Cargo.lock
168c37007c37ba0352dfed96d1c0cf2d3527552d8f4a8fc1c56c065459f28876  ./Cargo.toml
9a1b7db74ae5998b6246f44975c9328e73636a2759dc97d34e412a363422c848  ./Dockerfile
3b9649e05f3954650ae05347e8a097839a1934c179147dc3b48cb95815a6ef0c  ./LICENSE
38b29bcd53ff63596d2792a102c00819c4b9f4a061eb5b620f6ab0ed6491a965  ./Makefile
3c2fe984a6cb62143f0a58d4194e169a89f8caced51520100944b804ffc59669  ./README.md
b5f53cdb5fb227b599ead8cf7107868938b2ce136ada019e5bc75060fae392d5  ./config/polydb.env.example
a0f1922825d06ac25e3d22ada069b1f8d7412c4d6a40824615f928727229abde  ./docs/AUDIT.md
c0fc5c3bbf98cecd80d7a5218dd604d0cf58181e59dbbe9bc45d0370d1b16516  ./docs/DATA.md
5546cf560644ae73b031438b0ee6912c9cd1d0e8cb1a5af2846e900bd157ed4c  ./docs/ENVIRONMENT.md
fc335a601a800a96d2b88253c136d8389ead5201407dbea2278094ddb9b31315  ./docs/REFERENCE.md
c455940ee41c54fc1ded5bcacd6488c7faa55a851d74520bcdf0b38d3ef14dfb  ./docs/REPRODUCIBILITY.md
a6f14ce17e7d196af795dc54d9f167c7141b3c166cd0535fce5231c2c4ebadf1  ./docs/RUNBOOK.md
18d25103dd016d6ede7eaad8fed177405226729bdd0a577cf4770cb82c635eb1  ./docs/WALKTHROUGH.md
8f5d76f7e0d3605317b041a6c40e133c35e41933d5ac4b1db7f5884ce3b17e19  ./docs/evidence/clean-run-2026-11-11.log
f2c1727b9c1f2eb8637df18dd74b51215631c00e19a84e7cdadd219381f0edcd  ./scripts/check_env.sh
eea749706ddcb7bdb7b1e6c40c4677427dbb7c1912605e421f6a3d10744193c9  ./scripts/clean_run.sh
8414912b9c1b3109922f18682b381912484321f54d84f4f135f336a37de7d5f3  ./scripts/driver_checks.sh
91ad7c4c3123306f943206e921e9913ee0d226cb689feddd76c82ad98c9120fb  ./scripts/serve.sh
5911479c1cb460319a18e9db896d42b1bbe6e21ed23244df8040ad443e4e8c18  ./scripts/smoke.sh
a6fd3e7189025c62399374c704e819b634c7296b6ae6d3640954d3810d7bd26e  ./src/doc.rs
8ae2d4653300aba45c64e77f572777103551b4361c36eb2ff912b27ab3abf9cc  ./src/error.rs
acdb79a7b18cc4234345b2d44b820fd15c8c56ab929b8b1668903087ced1aaae  ./src/keys.rs
531ada1bd72f5321c056d2cad24f09d838a2802a84a324b1e4c44d6409f3222e  ./src/kv.rs
1d5ec677088aa418dd48e0e106b62b313d0e82bc291cf77ba9435241d0b021b2  ./src/lib.rs
29efabf6b7aba174929819f5f1727eba36e44e53ea158d62b7a24151c014d73f  ./src/main.rs
c76c9979d98c299790bd6567cce1dac86cfa9e05e9e6c324a308476490922af9  ./src/mongo.rs
c549a3fc3ebb2af1e9106e52fb2570558115e70a0f5a1294c383df16cbb56cae  ./src/pgwire.rs
570f04f4a2df42710388c65124a48b2833f4ed38bdab6bad63d6b9101a77a917  ./src/resp.rs
dfa3ddcb3387626314aabb9231a2b357475318afa5e210d894faf87b56138a02  ./src/server.rs
f19a25f3bf43cc767d43305d30da6f388b8d898a0829923725e11af4f5086050  ./src/sql.rs
3af59ed6aa88315bbef4e3272f34779a80f40333324b9e07280fe1fd0f127b2e  ./src/storage.rs
cd7c2c1fb1e630891b2b024abf622d6b8bb39718846a54188da2ef375016bd87  ./src/value.rs
45d2dc9a4931655056ecc6af68fd9bc3734d1aab67d9b7f5a094fb5bcc0b28e9  ./tests/drivers.py
d5768d0eb134be644f8d69635e7b3e72f06d832ddff30c045425308104da2433  ./tests/fixtures/smoke.commands
5539543dde908e71984fb7b831de4c315a01d07a4754ba953b41dfe867b2c230  ./tests/fixtures/smoke.expected
6c5404ce6eccfc1574404f76f23917d589f9aaf33243c1ab9c8ab5a39f093503  ./tests/hardening.rs
0608e5901a3dd0a687c4b2d82ab9915981b88e82886ef61df522f2eb9ec66dae  ./tests/integration.rs
b62b2255590e21bd095562e3871ea6970288b21c666860d4cdcd04c0b337490f  ./tests/requirements-drivers.txt
```
