# mzPeak

This R package implements an mzPeak reader from scratch in R using publicly available libraries. The high level API is exposed using an R6 class, `MZPeakFile`.

```R
require(mzpeak)

reader <- MZPeakFile$new("small.chunked.mzpeak")
```

## Metadata Access
The metadata tables are loaded and exposed as `arrow::Table` objects which are compatible with `dplyr`

```R
require(dplyr)

reader$spectra |> select("id", "index") |> collect()
```
```R
# A tibble: 48 × 2
   id                                          index
   <chr>                                       <int>
 1 controllerType=0 controllerNumber=1 scan=1      0
 2 controllerType=0 controllerNumber=1 scan=2      1
 3 controllerType=0 controllerNumber=1 scan=3      2
 4 controllerType=0 controllerNumber=1 scan=4      3
 5 controllerType=0 controllerNumber=1 scan=5      4
 6 controllerType=0 controllerNumber=1 scan=6      5
 7 controllerType=0 controllerNumber=1 scan=7      6
 8 controllerType=0 controllerNumber=1 scan=8      7
 9 controllerType=0 controllerNumber=1 scan=9      8
10 controllerType=0 controllerNumber=1 scan=10     9
# ℹ 38 more rows
```

`arrow::Table` play nicely with `dplyr` verbs, making no copies of the data until they must be materialized as plain R objects, delegating as many verbs to the Arrow C++ library as possible.

```R
reader$selected_ions |> filter(abs(`MS_1000744_selected_ion_mz_unit_MS_1000040` - 810.0) < 1) |> collect()
```
```R
# A tibble: 5 × 8
  source_index precursor_index MS_1000744_selected_ion…¹ MS_1000041_charge_st…² MS_1000042_intensity…³ ion_mobility ion_mobility_type
         <int>           <int>                     <dbl>                  <int>                  <dbl>        <dbl> <chr>
1            2               1                      811.                     NA               1994039.           NA NA
2            9               8                      811.                     NA               1554008.           NA NA
3           23              22                      811.                     NA                221684.           NA NA
4           36              35                      811.                     NA               1083631.           NA NA
5           43              42                      811.                     NA               2018922.           NA NA
# ℹ abbreviated names: ¹​MS_1000744_selected_ion_mz_unit_MS_1000040, ²​MS_1000041_charge_state, ³​MS_1000042_intensity_unit_MS_1000131
# ℹ 1 more variable:
#   parameters <large_list<
  tbl_df<
    value    :
      tbl_df<
        integer: integer
        float  : double
        string : character
        boolean: logical
      >
    accession: character
    name     : character
    unit     : character
  >
```

## Spectrum Data

Accessing spectrum signal data using `read_spectrum` retrieves all the points or peaks for that spectrum. Spectra are 1-indexed in R.

```R
mzIntens <- reader$read_spectrum(1)
head(mzIntens)
```
```R
        mz intensity
1 202.6066     0.000
2 202.6068  1938.117
3 202.6071  2572.839
4 202.6073  3392.107
5 202.6076  3729.591
6 202.6078  2819.127
```

Similarly, `read_spectrum_peaks` can be used to read separately written peak data for profile spectra that have their centroids stored adjacently.

## File-Level Metadata

File-level metadata entries are available as properties of the `spectrum_metadata` sub-object.

```R
 reader$spectrum_metadata$instrument_configuration_list$components
```
```R
[[1]]
  component_type order                                                                          parameters
1      ionsource     1 electrospray ionization, electrospray inlet, MS:1000073, MS:1000057, NA, NA, NA, NA
2       analyzer     2     fourier transform ion cyclotron resonance mass spectrometer, MS:1000079, NA, NA
3       detector     3                                              inductive detector, MS:1000624, NA, NA

[[2]]
  component_type order                                                                          parameters
1      ionsource     1 electrospray ionization, electrospray inlet, MS:1000073, MS:1000057, NA, NA, NA, NA
2       analyzer     2                                 radial ejection linear ion trap, MS:1000083, NA, NA
3       detector     3                                             electron multiplier, MS:1000253, NA, NA
```