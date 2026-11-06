# Everything needed to build, test and run the engine, the client and the bot.
{
  mkShell,
  rustPlatform,
  cargo,
  rustc,
  clippy,
  rustfmt,
  rust-analyzer,
  pkg-config,
  cmake,
  libopus,
  ffmpeg-headless,
  yt-dlp,
  deno,
  python3,
  chromium,
  bun,
  nixfmt,
}:

mkShell {
  packages = [
    # engine
    cargo
    rustc
    clippy
    rustfmt
    rust-analyzer
    pkg-config
    cmake
    libopus
    # tools the engine runs; see nix/package.nix
    ffmpeg-headless
    yt-dlp
    deno
    python3
    chromium
    # client and bot
    bun
    # flake
    nixfmt
  ];

  # lets rust-analyzer find the standard library's sources
  RUST_SRC_PATH = "${rustPlatform.rustLibSrc}";
  # as in the package: keeps glibc from holding on to the memory of every track the engine played
  MALLOC_ARENA_MAX = "2";
}
