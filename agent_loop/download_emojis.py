import urllib.request
import os

# Try alternative repository with pre-extracted files
BASE_URL = "https://raw.githubusercontent.com/Tarikul-Islam-Anik/Animated-Fluent-Emojis/master/Emojis/Smilies/"

# Map states to emoji file names
emoji_mapping = {
    'idle': 'Relieved Face.png',
    'connecting': 'Thinking Face.png',
    'thinking': 'Face with Monocle.png',
    'streaming': 'Beaming Face with Smiling Eyes.png',
    'reasoning': 'Nerd Face.png',
    'tool': 'Smiling Face with Sunglasses.png',
    'yielding': 'Sleeping Face.png',
    'done': 'Star-Struck.png',
    'error': 'Worried Face.png'
}

print("Downloading Animated Fluent Emojis...")
for state, filename in emoji_mapping.items():
    url = BASE_URL + filename.replace(' ', '%20')
    output = f'face_{state}.png'
    try:
        print(f"  {state}: {filename}")
        urllib.request.urlretrieve(url, output)
        size = os.path.getsize(output)
        print(f"    ✓ Downloaded {size} bytes -> {output}")
    except Exception as e:
        print(f"    ✗ Error: {e}")

print("\nDone!")
