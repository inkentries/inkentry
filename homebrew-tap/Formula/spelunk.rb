class Spelunk < Formula
  desc "Code intelligence for AI agents — persistent memory, code graph, search"
  homepage "https://spelunk.cloud"
  # TODO: update url and sha256 on each release
  url "https://github.com/spelunk-cloud/spelunk/releases/download/v0.8.0/spelunk-x86_64-apple-darwin.tar.gz"
  sha256 "TODO"
  license "EUPL-1.2"
  version "0.8.0"

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
