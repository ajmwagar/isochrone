#!/bin/zsh
exec /Users/ajmwagar/.local/bin/isochrone send \
  "Scarlett 18i20 4th Gen" \
  127.0.0.1:50040 \
  0.0.0.0:0 \
  5,6 \
  -6 2>&1 | tee /tmp/isochrone-sender-iterm.log
