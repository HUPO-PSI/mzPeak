# mzpeak

A Python of the mzPeak file format.

**NOTE**: This is a **work in progress**, no stability is guaranteed at this point.

## Usage

The `MzPeakFile` class handles the all the internal details of reading the archive.

```python
from mzpeak import MzPeakFile

reader = MzPeakFile("small.mzpeak")

spec = reader[2]
print(spec)
```
```python
{'id': 'controllerType=0 controllerNumber=1 scan=3',
 'ms level': 2,
 'time': 0.01121833361685276,
 'scan polarity': 1,
 'spectrum representation': 'MS:1000127',
 'spectrum type': 'MS:1000580',
 'lowest observed m/z': 231.3888397216797,
 'highest observed m/z': 1560.7198486328125,
 'number of data points': 485,
 'base peak m/z': 736.6370849609375,
 'base peak intensity': 161140.859375,
 'total ion current': 586279.0,
 'parameters': [],
 'number_of_auxiliary_arrays': 0,
 'mz_delta_model': None,
 'scans': [{'scan start time': 0.01121833361685276,
   'preset scan configuration': 3,
   'filter string': 'ITMS + c ESI d Full ms2 810.79@cid35.00 [210.00-1635.00]',
   'ion injection time': 7.993005275726318,
   'instrument_configuration_ref': 1,
   'parameters': [{'value': 0,
     'accession': None,
     'name': '[Thermo Trailer Extra]Monoisotopic M/Z:',
     'unit': None}],
   'scan_windows': [{'MS_1000501_scan_window_lower_limit_unit_MS_1000040': 210.0,
     'MS_1000500_scan_window_upper_limit_unit_MS_1000040': 1635.0,
     'parameters': []}]}],
 'precursors': [{'precursor_index': 1,
   'precursor_id': 'controllerType=0 controllerNumber=1 scan=2',
   'isolation_window': {'isolation window target m/z': 810.7894287109375,
    'isolation window lower offset': 809.7894287109375,
    'isolation window upper offset': 811.7894287109375,
    'parameters': []},
   'activation': [{'value': None,
     'accession': 'MS:1000133',
     'name': 'collision-induced dissociation',
     'unit': None},
    {'value': 35.0,
     'accession': 'MS:1000045',
     'name': 'collision energy',
     'unit': 'UO:0000266'}],
   'selected ion m/z': 810.789428710938,
   'peak intensity': 1994039.125,
   'parameters': []}],
 'index': 2,
 ...
}
```

The `MzPeakFile` has a `Sequence`-like interface for spectra. It supports random access and sequential iteration. `__getitem__` is an alias of `read_spectrum`.
For chromatograms, use the `get_chromatogram` method.

Currently, `read_spectrum` initiates a *cold read* on each call which does a small amount of reprocessing to find the rows of a Parquet row group which correspond to a spectrum
of interest, thus the loop
```python
for i in range(1, 10):
    foo(reader[i])

spec = reader[30]
foo(spec)
```
wastes some effort.

For efficient sequential scans, use the iterator returned by `iter(reader)`, an instance of `MzPeakFileIter`. It preserves its state and does not repeat effort when reading sequential entries. Additionally, it supports skipping ahead using the `seek` method, which helps when making short hops within the same row group.

```python
it = iter(reader)
for i, spec in enumerate(it):
    foo(spec)
    if i == 10:
        break

spec = it.seek(30)
foo(spec)
```

### Metadata access

#### File-Level Metadata
All the top-level metadata JSON is parsed and available in the `.file_metadata` dictionary

```python
from mzpeak import MzPeakFile

reader = MzPeakFile("small.mzpeak")

print(reader.file_metadata)
```
```python
{'file_description': {'contents': [{'name': 'MS1 spectrum',
    'accession': 'MS:1000579',
    'value': None,
    'unit': None},
   {'name': 'MSn spectrum',
    'accession': 'MS:1000580',
    'value': None,
    'unit': None}],
  'source_files': [{'id': 'RAW1',
    'location': 'file:///C:\\Users\\Joshua\\Dev\\learn-rust\\ffirawfilereader\\tests\\data',
    'name': 'small.RAW',
    'parameters': [{'name': 'SHA-1',
      'accession': 'MS:1000569',
      'value': 'b43e9286b40e8b5dbc0dfa2e428495769ca96a96',
      'unit': None},
     {'name': 'Thermo RAW format',
      'accession': 'MS:1000563',
      'value': None,
      'unit': None},
     {'name': 'Thermo nativeID format',
      'accession': 'MS:1000768',
      'value': None,
      'unit': None}]}]},
 'spectrum_count': 48,
 'instrument_configuration_list': [{'components': [{'component_type': 'ionsource',
     'order': 1,
     'parameters': [{'name': 'electrospray ionization',
       'accession': 'MS:1000073',
       'value': None,
       'unit': None},
      {'name': 'electrospray inlet',
       'accession': 'MS:1000057',
       'value': None,
       'unit': None}]},
    {'component_type': 'analyzer',
     'order': 2,
     'parameters': [{'name': 'fourier transform ion cyclotron resonance mass spectrometer',
       'accession': 'MS:1000079',
       'value': None,
       'unit': None}]},
    {'component_type': 'detector',
     'order': 3,
     'parameters': [{'name': 'inductive detector',
       'accession': 'MS:1000624',
       'value': None,
       'unit': None}]}],
   'parameters': [{'name': 'LTQ FT',
     'accession': 'MS:1000448',
     'value': None,
     'unit': None},
    {'name': 'instrument serial number',
     'accession': 'MS:1000529',
     'value': 'SN06061F',
     'unit': None}],
   'software_reference': 'Xcalibur',
   'id': 0},
  {'components': [{'component_type': 'ionsource',
     'order': 1,
     'parameters': [{'name': 'electrospray ionization',
       'accession': 'MS:1000073',
       'value': None,
       'unit': None},
      {'name': 'electrospray inlet',
       'accession': 'MS:1000057',
       'value': None,
       'unit': None}]},
    {'component_type': 'analyzer',
     'order': 2,
     'parameters': [{'name': 'radial ejection linear ion trap',
       'accession': 'MS:1000083',
       'value': None,
       'unit': None}]},
    {'component_type': 'detector',
     'order': 3,
     'parameters': [{'name': 'electron multiplier',
       'accession': 'MS:1000253',
       'value': None,
       'unit': None}]}],
   'parameters': [{'name': 'LTQ FT',
     'accession': 'MS:1000448',
     'value': None,
     'unit': None},
    {'name': 'instrument serial number',
     'accession': 'MS:1000529',
     'value': 'SN06061F',
     'unit': None}],
   'software_reference': 'Xcalibur',
   'id': 1}],
 'data_processing_method_list': [{'id': 'pwiz_Reader_Thermo_conversion',
   'methods': [{'order': 0,
     'software_reference': 'pwiz',
     'parameters': [{'name': 'Conversion to mzML',
       'accession': 'MS:1000544',
       'value': None,
       'unit': None}]}]},
  {'id': 'mzpeak_conversion1',
   'methods': [{'order': 1,
     'software_reference': 'mzpeak_prototyping_convert1',
     'parameters': [{'name': 'conversion options',
       'accession': None,
       'value': '-p -c -y -z -u small.mzML -o small.chunked.mzpeak',
       'unit': None}]}]}],
 'software_list': [{'id': 'Xcalibur',
   'version': '1.1 Beta 7',
   'parameters': [{'name': 'Xcalibur',
     'accession': 'MS:1000532',
     'value': None,
     'unit': None}]},
  {'id': 'pwiz',
   'version': '3.0.23307',
   'parameters': [{'name': 'ProteoWizard software',
     'accession': 'MS:1000615',
     'value': None,
     'unit': None}]},
  {'id': 'mzpeak_prototyping_convert1',
   'version': '0.1.0',
   'parameters': [{'name': 'custom unreleased software tool',
     'accession': 'MS:1000799',
     'value': 'mzpeak_prototyping_convert',
     'unit': None}]}],
 'sample_list': [{'id': '_x0031_',
   'name': '',
   'parameters': [{'name': 'sample name',
     'accession': 'MS:1000002',
     'value': 1,
     'unit': None}]}],
 'run': {'id': 'small',
  'default_data_processing_id': 'pwiz_Reader_Thermo_conversion',
  'default_instrument_id': 0,
  'default_source_file_id': 'RAW1',
  'start_time': '2005-07-20T19:44:22Z'},
 'spectrum_data_point_count': 1311}
```

The file index itself is parsed and accessible under `.file_index`:

```python
print(reader.file_index)
```
```python
FileIndex(files=[FileEntry(name='spectra_data.parquet', entity_type='spectrum', data_kind='data arrays'), FileEntry(name='spectra_peaks.parquet', entity_type='spectrum', data_kind='peaks'), FileEntry(name='spectra_metadata.parquet', entity_type='spectrum', data_kind='metadata'), FileEntry(name='chromatograms_metadata.parquet', entity_type='chromatogram', data_kind='metadata'), FileEntry(name='chromatograms_data.parquet', entity_type='chromatogram', data_kind='data arrays')], metadata={})
```

#### Observation-Level Metadata

All metadata is pre-loaded in Arrow arrays and presented for convenience in a set of `pandas.DataFrame`. Data are exposed in a zero-copy fashion, and can be passed along to other libraries and functions that work with Arrow, or where numerical, they can be zero-copy mapped to `numpy.ndarray` arrays. **Note**: Column names in these tables are automatically re-interpreted and de-inflected for convenience using the controlled vocabulary.

```python
from mzpeak import MzPeakFile

reader = MzPeakFile("small.mzpeak")

print(reader.spectra.head())
```
|   index | id                                         |   ms level |       time |   scan polarity | spectrum representation   | spectrum type   |   lowest observed m/z |   highest observed m/z |   number of data points |   base peak m/z |   base peak intensity |   total ion current | parameters   | auxiliary_arrays   |   number_of_auxiliary_arrays | mz_delta_model                                    |
|--------:|:-------------------------------------------|-----------:|-----------:|----------------:|:--------------------------|:----------------|----------------------:|-----------------------:|------------------------:|----------------:|----------------------:|--------------------:|:-------------|:-------------------|-----------------------------:|:--------------------------------------------------|
|       0 | controllerType=0 controllerNumber=1 scan=1 |          1 | 0.004935   |               1 | MS:1000128                | MS:1000579      |               200     |                1999.99 |                   19913 |         810.415 |           1.47122e+06 |         7.12632e+07 | []           | []                 |                            0 | [-2.21139275e-08  9.69754604e-11  6.05422863e-09] |
|       1 | controllerType=0 controllerNumber=1 scan=2 |          1 | 0.00789667 |               1 | MS:1000128                | MS:1000579      |               200.091 |                2000    |                   19800 |         810.545 |      183839           |         1.29013e+07 | []           | []                 |                            0 | [0.09090909]                                      |
|       2 | controllerType=0 controllerNumber=1 scan=3 |          2 | 0.0112183  |               1 | MS:1000127                | MS:1000580      |               231.389 |                1560.72 |                     485 |         736.637 |      161141           |    586279           | []           | []                 |                            0 | <NA>                                              |
|       3 | controllerType=0 controllerNumber=1 scan=4 |          2 | 0.0228383  |               1 | MS:1000127                | MS:1000580      |               236.047 |                1636.43 |                    1006 |         780.536 |       29161.9         |    441570           | []           | []                 |                            0 | <NA>                                              |
|       4 | controllerType=0 controllerNumber=1 scan=5 |          2 | 0.034925   |               1 | MS:1000127                | MS:1000580      |               203.222 |                1412.57 |                     837 |         578.986 |        8601.8         |    114332           | []           | []                 |                            0 | <NA>                                              |


```python
print(reader.precursors.head())
```

|   source_index |   precursor_index | precursor_id                               | isolation_window                                                                                                                                                                                  | activation                                                                                                                                                                             |
|---------------:|------------------:|:-------------------------------------------|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
|              2 |                 1 | controllerType=0 controllerNumber=1 scan=2 | {'isolation window target m/z': 810.7894287109375, 'isolation window lower offset': 809.7894287109375, 'isolation window upper offset': 811.7894287109375, 'parameters': array([], dtype=object)} | {'parameters': array([{'value': {'integer': None, 'float': None, 'string': None, 'boolean': None}, 'accession': 'MS:1000133', 'name': 'collision-induced dissociation', 'unit': None}, |
|                |                   |                                            |                                                                                                                                                                                                   |        {'value': {'integer': None, 'float': 35.0, 'string': None, 'boolean': None}, 'accession': 'MS:1000045', 'name': 'collision energy', 'unit': 'UO:0000266'}],                     |
|                |                   |                                            |                                                                                                                                                                                                   |       dtype=object)}                                                                                                                                                                   |
|              3 |                 1 | controllerType=0 controllerNumber=1 scan=2 | {'isolation window target m/z': 837.3446044921875, 'isolation window lower offset': 836.3446044921875, 'isolation window upper offset': 838.3446044921875, 'parameters': array([], dtype=object)} | {'parameters': array([{'value': {'integer': None, 'float': None, 'string': None, 'boolean': None}, 'accession': 'MS:1000133', 'name': 'collision-induced dissociation', 'unit': None}, |
|                |                   |                                            |                                                                                                                                                                                                   |        {'value': {'integer': None, 'float': 35.0, 'string': None, 'boolean': None}, 'accession': 'MS:1000045', 'name': 'collision energy', 'unit': 'UO:0000266'}],                     |
|                |                   |                                            |                                                                                                                                                                                                   |       dtype=object)}                                                                                                                                                                   |
|              4 |                 1 | controllerType=0 controllerNumber=1 scan=2 | {'isolation window target m/z': 725.362060546875, 'isolation window lower offset': 724.362060546875, 'isolation window upper offset': 726.362060546875, 'parameters': array([], dtype=object)}    | {'parameters': array([{'value': {'integer': None, 'float': None, 'string': None, 'boolean': None}, 'accession': 'MS:1000133', 'name': 'collision-induced dissociation', 'unit': None}, |
|                |                   |                                            |                                                                                                                                                                                                   |        {'value': {'integer': None, 'float': 35.0, 'string': None, 'boolean': None}, 'accession': 'MS:1000045', 'name': 'collision energy', 'unit': 'UO:0000266'}],                     |
|                |                   |                                            |                                                                                                                                                                                                   |       dtype=object)}                                                                                                                                                                   |
|              5 |                 1 | controllerType=0 controllerNumber=1 scan=2 | {'isolation window target m/z': 558.8689575195312, 'isolation window lower offset': 557.8689575195312, 'isolation window upper offset': 559.8689575195312, 'parameters': array([], dtype=object)} | {'parameters': array([{'value': {'integer': None, 'float': None, 'string': None, 'boolean': None}, 'accession': 'MS:1000133', 'name': 'collision-induced dissociation', 'unit': None}, |
|                |                   |                                            |                                                                                                                                                                                                   |        {'value': {'integer': None, 'float': 35.0, 'string': None, 'boolean': None}, 'accession': 'MS:1000045', 'name': 'collision energy', 'unit': 'UO:0000266'}],                     |
|                |                   |                                            |                                                                                                                                                                                                   |       dtype=object)}                                                                                                                                                                   |
|              6 |                 1 | controllerType=0 controllerNumber=1 scan=2 | {'isolation window target m/z': 812.3253173828125, 'isolation window lower offset': 811.3253173828125, 'isolation window upper offset': 813.3253173828125, 'parameters': array([], dtype=object)} | {'parameters': array([{'value': {'integer': None, 'float': None, 'string': None, 'boolean': None}, 'accession': 'MS:1000133', 'name': 'collision-induced dissociation', 'unit': None}, |
|                |                   |                                            |                                                                                                                                                                                                   |        {'value': {'integer': None, 'float': 35.0, 'string': None, 'boolean': None}, 'accession': 'MS:1000045', 'name': 'collision energy', 'unit': 'UO:0000266'}],                     |
|                |                   |                                            |                                                                                                                                                                                                   |       dtype=object)}                                                                                                                                                                   |

```python
print(reader.selected_ions.head())
```
|   source_index |   precursor_index |   selected ion m/z |   peak intensity | parameters   |
|---------------:|------------------:|-------------------:|-----------------:|:-------------|
|              2 |                 1 |            810.789 |      1.99404e+06 | []           |
|              3 |                 1 |            837.345 | 999937           | []           |
|              4 |                 1 |            725.362 | 313667           | []           |
|              5 |                 1 |            558.869 | 202178           | []           |
|              6 |                 1 |            812.325 |      1.16189e+06 | []           |

```python
print(reader.chromatograms.head())
```
|   index | id   |   scan polarity | chromatogram type   | parameters   | auxiliary_arrays   |   number_of_auxiliary_arrays |
|--------:|:-----|----------------:|:--------------------|:-------------|:-------------------|-----------------------------:|
|       0 | TIC  |               0 | MS:1000235          | []           | []                 |                            0 |

### Time axis queries

Using the embedded indices, we can look up spectra by time, or

```python
from mzpeak import MzPeakFile

reader = MzPeakFile("small.mzpeak")

spec = reader.time[0.3]
print(spec)
```
```python
{'id': 'controllerType=0 controllerNumber=1 scan=31',
 'time': 0.30370333790779114,
 'number_of_auxiliary_arrays': 0,
 'scan polarity': 1,
 'spectrum type': 'MS:1000580',
 'number of data points': 938,
 'base peak m/z': 780.756591796875,
 'base peak intensity': 41109.4765625,
 'total ion current': 379009.78125,
 'lowest observed m/z': 236.30441284179688,
 'highest observed m/z': 1635.1453857421875,
 'parameters': [{'name': 'centroid spectrum',
   'accession': 'MS:1000127',
   'value': None,
   'unit': None}],
 'index': 30,
 'm/z array: ...
 }
 ```

 ```python
 print([(s['index'], s['time'], len(s['intensity array'])) for s in reader.time[0.3:0.35]])
 ```
 ```python
 [(30, 0.30370333790779114, 938),
  (31, 0.31564998626708984, 653),
  (32, 0.32852667570114136, 702),
  (33, 0.34291499853134155, 587)]
 ```

## Optional Features

### SQL Interface

With `datafusion` installed, you can get a SQL interface to the raw Arrow tables:

```python
ctx = reader.to_sql()
ctx.sql("""SELECT * FROM selected_ions WHERE ABS("selected ion m/z" - 810.78) < 0.01""")
```

### MS-Numpress Support

Install with the `numpress` feature enabled or install `pynumpress` separately to enable MS-Numpress decompression.

## To be implemented

- Filtering by m/z while filtering by index range
- Slicing chromatograms
- More efficient block caching