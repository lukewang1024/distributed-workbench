#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
mkdir -p dist
./node_modules/esbuild/bin/esbuild src/main.tsx --bundle --minify --format=iife --outfile=dist/app.js
cp src/index.html dist/index.html
