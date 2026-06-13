cargo_env = environment()
cargo_target_dir = meson.project_build_root() / 'target'

custom_target(
  'cycle-bin',
  build_by_default: true,
  build_always_stale: true,
  output: 'cycle',
  console: true,
  install: true,
  install_dir: bindir,
  command: [
    find_program('bash'),
    meson.project_source_root() / 'build-aux' / 'cargo-build.sh',
    meson.project_source_root(),
    cargo_target_dir,
    rust_target,
    '@OUTPUT@',
  ],
  env: cargo_env,
)
