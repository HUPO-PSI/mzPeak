import sys
import json

import mzpeak


path = sys.argv[1]
reader = mzpeak.MzPeakFile(path)

json.dump(reader.file_index.to_json(), sys.stdout, indent=2, sort_keys=True)