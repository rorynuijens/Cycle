project('cycle',
  version: '0.4.0',
  meson_version: '>= 1.0',
  license: 'MIT',
)

prefix  = get_option('prefix')
bindir  = prefix / get_option('bindir')
datadir = prefix / get_option('datadir')

application_id = 'io.github.rorynuijens.Cycle'

profile = get_option('profile')
if profile == 'release'
  rust_target  = 'release'
  cargo_flags  = [ '--release' ]
else
  rust_target  = 'debug'
  cargo_flags  = []
endif

cargo = find_program('cargo', required: true)

subdir('data')
subdir('src')
