# frozen_string_literal: true

require "json"
require "socket"

dir, version, port_file = ARGV
server = TCPServer.new("127.0.0.1", 0)
url = "http://127.0.0.1:#{server.addr[1]}"
repo = "/repos/EzraCerpac/dots/releases"
download = "/EzraCerpac/dots/releases/download/v#{version}"
assets = File.readlines(File.join(dir, "SHA256SUMS")).map.with_index(1) do |line, index|
  sum, name = line.split
  path = File.join(dir, name)
  { id: index, name: name, size: File.size(path), digest: "sha256:#{sum}",
    url: "#{url}#{repo}/assets/#{index}", browser_download_url: "#{url}#{download}/#{name}" }
end
release = { tag_name: "v#{version}", draft: false, prerelease: false,
            created_at: "2026-01-01T00:00:00Z", assets: assets }

File.write(port_file, url)
loop do
  client = server.accept
  body = nil
  begin
    method, request, = client.gets.to_s.split
    path = request.to_s.split("?", 2).first
    while (header = client.gets) && header != "\r\n"
      # Consume the request headers before sending the response.
    end
    asset = assets.find { |entry| [entry[:url], entry[:browser_download_url]].include?("#{url}#{path}") }
    if asset
      body = File.open(File.join(dir, asset[:name]), "rb")
      size = asset[:size]
      content_type = "application/octet-stream"
    else
      body = case path
             when "#{repo}/tags/v#{version}", "#{repo}/latest" then JSON.generate(release)
             when repo then JSON.generate([release])
             else ""
             end
      size = body.bytesize
      content_type = "application/json"
    end
    status = size.positive? ? "200 OK" : "404 Not Found"
    client.write("HTTP/1.1 #{status}\r\nContent-Type: #{content_type}\r\nContent-Length: #{size}\r\nConnection: close\r\n\r\n")
    unless method == "HEAD"
      if asset
        while (chunk = body.read(64 * 1024))
          client.write(chunk)
        end
      else
        client.write(body)
      end
    end
  rescue Errno::EPIPE, Errno::ECONNRESET, Errno::ENOTCONN
    # A probing client can disconnect after reading the headers.
  ensure
    body.close if body.is_a?(File)
    client.close
  end
end
