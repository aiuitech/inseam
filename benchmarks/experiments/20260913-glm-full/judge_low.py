"""Select low effort, then execute the pinned upstream scorer unchanged."""
import functools
import runpy
import sys
from pathlib import Path
sys.path.insert(0, str(Path.cwd()))
import src.llm
src.llm.get_llm = functools.partial(src.llm.get_llm, reasoning_level='low')
src.llm.get_cheap_llm = functools.partial(src.llm.get_cheap_llm, reasoning_level='low')
runpy.run_module('src.scripts.answer_evaluation.metrics_based_eval', run_name='__main__')
