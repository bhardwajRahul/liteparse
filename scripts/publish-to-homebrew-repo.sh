#/bin/bash

echo "Installing homebrew-npm-noob tool"
uv tool install --upgrade homebrew-npm-noob
echo "Setting up repository locally"
git clone https://github.com/run-llama/homebrew-liteparse
cd homebrew-liteparse
mkdir -p Formula/
echo "Generating HomeBrew Formula"
noob @llamaindex/liteparse > Formula/llamaindex-liteparse.rb
echo "Pushing to GitHub"
git add .
git commit -m "Automated HomeBrew Release for liteparse"
git push -u origin main
echo "Removing local copy"
cd ..
rm -rf homebrew-liteparse
