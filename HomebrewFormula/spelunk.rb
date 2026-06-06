class Spelunk < Formula
  desc "Code intelligence for AI agents — persistent memory, code graph, search"
  homepage "https://github.com/spelunk-cloud/spelunk"
  license "MIT"
  version "0.8.0"

  # Tap this repo directly:
  #   brew tap spelunk-cloud/spelunk https://github.com/spelunk-cloud/spelunk
  #   brew install spelunk-cloud/spelunk/spelunk
  #
  # sha256 values are updated automatically by the release workflow on each tag.
  # See .github/workflows/release.yml (update-homebrew-formula job).
  # To update manually: download the tarballs, run `shasum -a 256 <file>`, and
  # replace the sha256 values below, then open a PR.

  on_macos do
    on_arm do
      url "https://github.com/spelunk-cloud/spelunk/releases/download/v#{version}/spelunk-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "TODO_aarch64_apple_darwin"
    end
    on_intel do
      url "https://github.com/spelunk-cloud/spelunk/releases/download/v#{version}/spelunk-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "TODO_x86_64_apple_darwin"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/spelunk-cloud/spelunk/releases/download/v#{version}/spelunk-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "TODO_aarch64_linux"
    end
    on_intel do
      url "https://github.com/spelunk-cloud/spelunk/releases/download/v#{version}/spelunk-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "TODO_x86_64_linux"
    end
  end

  def install
    bin.install "spelunk"
    bin.install "spelunk-server"
  end

  test do
    system "#{bin}/spelunk", "--version"
  end

  def caveats
    <<~EOS
      Run `spelunk init` in a project directory to get started.
      To start the local server: `spelunk server start`
    EOS
  end
end
