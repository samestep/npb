# Changelog for `npb`

## 1.2.0

- Fix lack of quoting for `-p` arguments in reproduction commands ([`8922905`](https://github.com/samestep/npb/commit/89229055502b88898d21e4731737456f5049598e))
- Fix not all transitive dependents being built after a failed build later succeeds ([`06e7098`](https://github.com/samestep/npb/commit/06e70981d2cd967d426dec25d4d86278c3e81843))
- Add a repeatable `--overlay` CLI argument ([#5](https://github.com/samestep/npb/pull/5))

## 1.1.3

- Improve performance when searching for failed dependencies ([`7e4d47a`](https://github.com/samestep/npb/commit/7e4d47a26a4f05a753eb270b5147a052dd216a30))

## 1.1.2

- Fix `instantiate` counts resetting to zero when finished ([`b8a5051`](https://github.com/samestep/npb/commit/b8a505160d84692333b061dfcb786fa665030ca0))

## 1.1.1

- Allow attr paths containing functions that can be auto-called with `{ }` ([`3287434`](https://github.com/samestep/npb/commit/32874341be93f0a85d972301e15f62bec4f07f36))
- Fix building attr paths with quotes in them ([`2af27f5`](https://github.com/samestep/npb/commit/2af27f5f32e38569e458eb0e27c4eab134e014b2))

## 1.1.0

- Add a `--package` override CLI argument ([#3](https://github.com/samestep/npb/pull/3))
