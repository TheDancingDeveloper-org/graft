`pr-batch` reads the batch line with `GetLineConfig` and refuses when
`--checks` disagrees with the line's checks mode. An older server that
answers unimplemented keeps the previous behaviour and prints a one-line
warning. (WI-2614)
